# Remaining work: diagnostics unification (#168)

Goal: every user-facing DSL/CLI error behind one keyed constructor;
`anyhow` only as carrier where a boundary forces it; drop the dep per
crate when no `anyhow::` path remains. Changes stay uncommitted for
review until the user explicitly asks for a commit.

Legend: `- [ ]` open, `- [x]` done, `- [!]` stay (closed as permanent).
Gates per bulk: `cargo test` green, `cargo clippy` no new warnings,
`rustfmt --edition 2024` clean on touched files. Batches stay
file-sized; one plan-log entry per bulk in
`.opencode/plans/diagnostics-unification.md`.

## A. Mechanical bulks

### A1. `core/exec/handlers.rs` leaf-to-root flips (~284 sites, 61 fns)
All sites already use keyed ctors; flip signatures to
`DiagnosticResult`, anyhow parents hold via blanket `From`
(`Ok(f(...)?)` at boundaries). Blocked only at the top by B1.

- [x] H1 archive cluster (58): `collect_archive_tree`,
      `append_archive_tree`, `archive`, `resolve_archive_src`,
      `unarchive`
- [x] H2a read/write cluster (30): `write`, `append`, `read`,
      `read_line`
- [x] H2b fs-ops cluster (35): `symlink`, `remove`, `mkdir`,
      `set_executable`, `ls`, `install`, `append_line`,
      `push_into`, `copy`, `copy_git`, `place_file_in_dir`,
      `link_base_name`
- [x] H3 run cluster (41): `run`, `run_argv`, `run_pty`,
      `resolve_run_exec_argv`, `flatten_exec_value`,
      `resolve_io_streams`, `top_level_flow`,
      `drain_stdin_bounded`, `exact_stdout_bytes`,
      `tty_request`, `resolve_pipe_handle`
- [x] H4 call/dispatch cluster (29): `execute_call_entry`,
      `resolve_call_entry`, `gate_call`, `block_value`,
      `build_script_entry`, `for_loop`, `while_loop`,
      `dispatch_break`, `dispatch_continue`, `with_io_block`,
      `exit`, `workdir`, `sleep`
- [x] H5 async cluster (29): `assign_capture`,
      `dispatch_async_block`, `dispatch_assign_async`,
      `resolve_task_entry`, `resolve_task_entry_by_id`,
      `dispatch_await`, `dispatch_await_capture`,
      `await_task_entry`, `dispatch_cancel`, `disarm_deadline`,
      `isolated_inherit_env`
- [x] H6a assert cluster (35): `assert_equality`,
      `assert_contains`
- [x] H6b replace/prompt cluster (18): `replace`, `prompt`,
      `bind_prompt_var`

### A2. `core/exec/import.rs` boundary returns (9 sites)
- [x] Flip with B1: `import_unknown_function`,
      `import_stateful_harvest`, `import_type_collision`,
      `import_flow_crosses_boundary` x2, `import_unknown_type`
      host-module probe, `nested_cause`
- [x] `pop_scope` `anyhow::Error::from` shim (standard interop;
      drops when B1 flips)
- [x] `import_const_forward_failed` (needs B5, not B1;
      done in B5: `resolve_const_ref` propagates natively)

### A3. `core/exec/mcp.rs` (4 sites)
- [x] Flip with B1: `mcp_export_type_unknown`,
      break/continue/return-outside

### A4. `core/exec/mod.rs` public runner API
- [x] BREAK/CONTINUE/RETURN-outside returns (`finish_run`
      flips to `DiagnosticResult`; 3 outer
      `anyhow::Error::new(...)` wrappers dropped).
- [ ] `run_steps_with_context*` family. Needs B1 decision
      (public API affects CLI/mcp/doc-tests callers; `finish_run`
      flip holds via `From` with no caller changes, but the
      public family itself plus `error_with_snapshot`/
      `enrich_lazy_error` Diagnostic-outer shape stays open),
      then mechanical.

### A5. `oxdock-build` fn flips (dep stays; see below)
- [ ] Flip remaining fns to `DiagnosticResult`, dropping the
      `anyhow::Error::new(ctor(...))` construction sites
      (`assets.rs` 18 sites already use foreign-error ctors;
      `lib.rs` already anyhow-free)
- Note: the `anyhow` dep CANNOT be dropped: the
  `build.*` ctors take `reason: &anyhow::Error`
  (`diagnostics.rs:26-241`), and that surface is permanent
  while `oxdock-embed` stays anyhow (Section D). Any
  `&anyhow::Error` in a signature requires `anyhow` in
  `[dependencies]`; removal would fail `cargo check` with
  unresolved crate. `oxdock-build` therefore lives in
  Section C's permanent carrier list, not its removal list.

### A6. `oxdock-mcp/server.rs` (5 sites)
- [x] Flip with B1: `session_lock_poisoned_reload`,
      guard-workspace/script-root, `export_no_entries`,
      status-name-reserved

## B. Decisions (record outcome on the Decision line)

- [x] B1 `execute_steps` keystone. Settle together: `step_error`
      Diagnostic-native (position via `with_span` sync),
      `error_with_snapshot` renderer (`core/exec/mod.rs:168`),
      `ThreadJoinHandle::reemit_error` + panic capture
      (`steps.rs:84,135`), `BackgroundHandle`/`ThreadHandle`
      over `Diagnostic`, `ExitStatus`-over-`Diagnostic`
      (Display is platform-specific, never hardcode).
      Unlocks A1-A4, A6, all of A2 except const-forward.
      Decision: Approved incremental. `step_error` attaches
      `Span` via `with_span` on reclaimed `Diagnostic` and keeps
      the `step N` message prefix byte-identical during migration.
      Keyed failures keep their own diagnostic as the head error
      with the snapshot section attached via native
      `Diagnostic::with_snapshot` provenance (frames replayed via
      `with_contexts`, exit code attached only when the chain
      lacks one); `dsl.eval.snapshot_context` is reserved for
      unkeyed fallbacks. `ThreadJoinHandle`
      stores `Diagnostic` (`Clone` removes flatten re-emit) with panic
      payloads behind `dsl.eval.async_thread_panicked`.
      `BackgroundHandle` flips to `DiagnosticResult` now (process crate
      already depends on diagnostics, no cycle); `ProcessManager`
      run/spawn surface stays anyhow until B2. `ExitStatus` ctors
      migrate to numeric code fields via one `describe_exit_status`
      helper, never hardcoded `Display`.
       (B1i amendment: the `BackgroundHandle` flip additionally
       requires Diagnostic-native storage for the multi-frame
       `step N` call path. `step_error` still chains outer frames
       as anyhow `.context` layers and
       `ThreadJoinHandle::reemit_error` replays `thread_context`
       the same way, which cannot cross a `DiagnosticResult`
       boundary. The flip bulk must add frame storage on
       `Diagnostic` itself and route `thread_context` plus the
       step wraps through it; the "waiting only on the span sync"
       premise in B1f/B1g is corrected accordingly.)
- [ ] B2 `ProcessManager` trait (`contract.rs`,
      `shell_manager.rs`, `mock.rs`, `synthetic.rs`,
      `child.rs`). Flip trait to `DiagnosticResult` OR declare
      anyhow-at-the-sys-boundary permanent.
      Decision: Split. `BackgroundHandle` flips to
      `DiagnosticResult` inside B1 (no cycle; `builder.rs`
      already returns it). `ProcessManager` run/spawn/argv
      surface stays anyhow through B1; a dedicated B2 bulk
      decides the full-trait flip versus a permanent
      sys-boundary carrier.
- [!] B3 `RemoteRunner` trait + `TransferStage`
      (`net session.rs` 36 sites, `core/remote.rs`
      `run_remote_block`). Redesign OR close as permanent
      (concrete-error params risk flattening chained
      `RpcServiceError`/`FrameEncodeError` io, proven in the
      fetch bulk).
      Decision: Close as permanent. `RemoteRunner` +
      `TransferStage` stay anyhow; concrete-error params risk
      flattening chained `RpcServiceError`/`FrameEncodeError`
      io. Revisit only with a typed-error redesign proposal.
- [!] B4 `oxdock-fs` exclusion (~200 sites: `io.rs` 68,
      `backend.rs` 37, `copy.rs` 27, `git/` 27, `install.rs`
      12, `path.rs` 10, ...). (a) Permanent exclusion: fs
      keeps literals, stays anyhow; OR (b) move shared keyword
      constants to a leaf crate to break the parser cycle,
      then standard bulk conversion. Largest scope decision
      left.
      Decision: Permanent exclusion. `oxdock-fs` keeps
      literals and stays anyhow (affirms #345). No
      keyword-constant move; the ~200 sites stay out of
      scope.
- [x] B5 `ForwardError` redesign (small, self-contained, no
      input needed): `chase_const_forward` ->
      `DiagnosticResult`; unblocks `import_const_forward_failed`
      + `check_ctx_wrap` sites (`import.rs`, `static_check.rs`
      :1022,:5166).
      Decision: Approved. Small bulk, runs after B1 with no
      further input.
- [ ] B6 `oxdock-utils/tar.rs` pack/unpack (3 sites): needs io
      ctors + `apply`-callback migration design first.
      Decision: Deferred design. Runs after the H-bulks once
      the io-ctor and callback-migration shape is proposed.
- [x] B7 Closure cascade: `write_stdout`/`write_stderr`
      (`core/io.rs:368,392`) + 10+ inline closures in
      `handlers.rs`/`remote.rs`. Flip as one cascade with
      callers, inside B1's blast radius.
      Decision: Approved as a B1 cascade. Flips together with
      callers inside B1, not as a separate bulk.

## C. Per-crate closure (grep `anyhow::` paths -> remove dep -> gates)

- [x] `oxdock-diagnostics` (anyhow dev-only already)
- [ ] `oxdock-pty`
- [ ] `oxdock-process` (after B2; windows-only `child.rs`
      needs B9)
- [ ] `oxdock-pipe`
- [!] Keep permanently as carrier/macro-boundary (staged
      retirement, not failure): `core`, `parser`, `cli`,
      `mcp`, all three plugins, `oxdock-build` (foreign
      `&anyhow::Error` ctor params are permanent while
      `embed` stays anyhow), `fs` (per B4), `embed`

## D. Stays to record as final (decided, just log them)

- [ ] `exit_code_of(&anyhow::Error)` (must survive transports)
- [ ] `check_value_at` / `tag.rs` generated extractors
      (passthrough ctors at call sites; closes
      `toc_option_check_failed` normalization as won't-fix;
      recording this final SUPERSEDES and closes the
      FOLLOW-UP at `diagnostics-unification.md:2957`, which
      must be marked closed at that time so the two plans
      do not contradict)
- [ ] `#[oxdock_func]` fns everywhere + func-macro
      `::anyhow::bail!` (macro boundary)
- [ ] `russh Handler::Error = anyhow::Error`
      (`ssh auth.rs`/`runtime.rs`)
- [ ] `macro_input.rs` proc-macro boundary
- [ ] `oxdock-embed` gather fns (fs callers)
- [ ] `PumpDownstreamClosed` sentinel + pump thread fns
      (thread-error design)
- [ ] `eval_int_arg_resolve` + `std_pipe_{read,create}_failed`
      (inners reach sourced `os_pipe_creation_failed`)
- [ ] CLI help/version `bail!` (not error diagnostics)
- [ ] Out of scope: test-harness / logic-tests / fixture /
      doc-tests / xtask / render tooling
- [ ] B9 Windows-only code (`process/child.rs` job objects,
      `cli` re-exec x5, `assign_job` + unused
      `job_assign_failed` ctor): verify on Windows or
      accept-as-is
- [!] B3 `RemoteRunner` + `TransferStage` stay anyhow
      (concrete-error params risk flattening chained io;
      decided in B3, logged here for the final audit)
- [!] B4 `oxdock-fs` keeps literals and stays anyhow
      (affirms #345; decided in B4, logged here for the
      final audit)

## E. Suggested order

1. Decide B1 -> unlocks A1-A4, A6, A2.
2. Decide B4 -> settles ~200 sites (fs chapter: convert or
   close as permanent).
3. Decide B2/B3 -> end-state of process + net session.
4. Run bulks (H1-H6b file-sized), B5/B6 designs, B7 inside B1.
5. Section C per-crate closure.
6. Section D recorded final; future audit reads zero open items.

## Log

- B1a (2026-11-26): keystone helpers without signature flips.
  Added `exit_code_of_diag`, `exit_status_code`,
  `describe_exit_status` (`core/exec/exit.rs`); new ctors
  `eval_async_thread_panicked`, `eval_run_snapshot_context`;
  normalized 4 `ExitStatus` ctors to the shared helper;
  flipped `ThreadJoinHandle` to store `Diagnostic` (clone re-emit,
  panic via new ctor, `ExitRequest` re-attached only when the
  chain lacks one); updated `async_failure_bubbles_status`
  assertion for numeric status. Gates: `cargo test -p oxdock-core
  --lib` 413 passed, `cargo clippy -p oxdock-core --all-targets`
  clean, `rustfmt --edition 2024` clean on touched files.
  Remaining B1: `step_error` span sync, `error_with_snapshot`
  Diagnostic outer, `BackgroundHandle` trait flip.

- B1a follow-up (2026-11-26): field-key constants for the B1
  family. Added `FIELD_ARGV`, `FIELD_CODE`, `FIELD_COMMAND`,
  `FIELD_DETAIL`, `FIELD_SNAPSHOT`, `FIELD_STATUS` at the top of
  `core/exec/diagnostics.rs` and replaced all 20 literal sites, so
  no new hardcoded keys ship with B1a. Mirrors the `FIELD_*`
  pattern in `oxdock-process`, `oxdock-fs`, and the net plugin.
  Full key-constant migration (reason 99x, path 49x, etc.) stays a
  later bulk. Gates: `cargo test -p oxdock-core --lib` 413 passed,
  clippy clean, rustfmt clean.

- B1b (2026-11-26): review amendments. `ThreadJoinHandle::reap`
  reclaims wrapped diagnostics via `e.chain().find_map` clone
  instead of outermost-only `downcast`, so `.context(...)`
  wrappers no longer strip key/span/fields; genuinely unkeyed
  errors keep the `eval_dispatch_failed` fallback. Exit-status
  schema split: the 4 exit ctors emit `code` when
  `exit_status_code` returns `Some`, `status` (platform
  rendering) only on signal termination; `async_failure_bubbles_status`
  asserts the exact `"exited with status 7"` contract. Field
  constants completed in every touched constructor
  (`FIELD_DECLARES`, `FIELD_GOT`, `FIELD_REASON`, `FIELD_STEP`,
  `FIELD_TARGET`, `FIELD_TASK`, `FIELD_VAR`, `FIELD_YIELD`;
  `FIELD_` constructors verified literal-free). Plan hygiene:
  duplicate B2 line removed, B2/B3/B4/B5/B6/B7 decisions
  recorded, log dates corrected. Gates: `cargo test -p oxdock-core`
  413 + 629 + 12 + 2 passed, clippy clean, rustfmt clean.

- B1c (2026-11-26): second review amendments. `reap` also
  captures outer context text above the reclaimed diagnostic
  (`thread_context`, `": "`-joined, re-applied as one anyhow
  layer) so `step N` location survives until the `step_error`
  span sync; fallback uses new `eval_dispatch_failed_owned`
  (owned chain rides as `with_source` via
  `into_boxed_dyn_error`). `reemit_error` attaches `ExitRequest`
  via `with_source` only when sourceless, else relays at the
  anyhow carrier layer (pre-B1 shape), never clobbering an
  existing source. Plan: B3/B4 marked `- [!]` with Section D
  stay entries. Tests: 4 `thread_handle_tests` (panic key,
  wrapped key + outer text, sourceless key + exit, sourced
  text + exit) and 2 exit-field schema tests (numeric `code`,
  Unix signal `status`). Gates: `cargo test -p oxdock-core`
  419 + 629 + 12 + 2 passed, clippy clean, rustfmt clean.

- B1d (2026-11-26): third review amendments, lossless chaining.
  `ExitRequest` gains an optional chained source
  (`new`/`with_source`/`code()`; equality ignores the source,
  mirroring `Diagnostic`) so an exit code attaches behind a
  diagnostic without dropping its cause: chain is
  `Diagnostic` -> `ExitRequest` -> prior cause, and both
  `exit_code_of` walkers match the request node directly.
  `Diagnostic::shared_source` exposes the `Arc` cause for
  re-chaining. `reemit_error` has no destructive branch left:
  key, span, fields, exit code, and original source all survive
  as typed nodes, which also keeps it working once
  `BackgroundHandle` flips to `DiagnosticResult`. `reap` skips
  `ExitRequest` nodes when capturing outer frames (no text
  duplication) and `thread_context` is now `Vec<String>`
  replayed innermost-first, preserving individual frames.
  `eval_dispatch_failed_owned` keeps the chain through a
  source-forwarding adapter (`into_boxed_dyn_error` was proven
  to erase downcasting per anyhow's own docs). Tests replaced
  the false-positive with `sourced_diagnostic_keeps_key_source
  _and_exit_code` (truly sourced fixture, all three asserts
  structural) plus an unkeyed-fallback test; wrapped-context
  test now pins ordered frames. Section C permanent carrier
  marked `- [!]`. Gates: core 421 + 629 + 12 + 2, diagnostics
  8, cli `process_exit_code`, net-plugin 38, clippy clean,
  rustfmt clean.

- B1e (2026-11-27): fourth review amendments, no-duplication
  relay. Removed `OwnedAnyhow` / `eval_dispatch_failed_owned`:
  the adapter violated the `Error` display/source contract
  (message already holds `{reason:#}` while the source
  re-rendered `{:#}` and re-yielded the chain head, tripling
  `{err:#}`). Unkeyed threads now use `eval_dispatch_failed(&e)`
  (sourceless, like every other foreign boundary). Bare
  `ExitRequest` threads relay cleanly: `reap` stores outer
  frames with `thread_error = None`, `reemit_error` branches on
  `(error, exit)` with a direct `ExitRequest::new(code)` arm,
  and `try_wait`/`wait` trigger on either slot. Tests pin
  `{err:#}` exactly (sourced `cancelled: EXIT 7: gone`,
  unkeyed `step 3: denied`, bare `step 2: EXIT 7`) plus a
  no-`Diagnostic` assert on the bare path. Gates: core
  422 + 629 + 12 + 2, diagnostics 8, cli `process_exit_code`,
  net-plugin 38, clippy clean, rustfmt clean.

- B1f (2026-11-27): keyed exit diagnostic (H4 share). Added
  `eval_exit_requested(code)` (`dsl.eval.exit_requested`,
  message byte-identical to the historic carrier, `code` field,
  no source so `{err:#}` renders once). Both exit-code walkers
  recognize it (field read) alongside `ExitRequest`, so
  `handlers::exit` and the `ThreadJoinHandle` bare-exit arm now
  relay the keyed diagnostic instead of the bare request.
  Bare-exit test now asserts the key + `code` field + single
  `{err:#}`; new walker test pins sourceless carry and context
  passthrough. `BackgroundHandle` trait flip sequenced after
  `step_error` span sync within B1: outer `thread_context`
  frames have no Diagnostic-native slot until the span sync
  lands (folding them into the message would reintroduce
  flattening). Gates: core
  423 + 629 + 12 + 2, diagnostics 8, cli `process_exit_code`,
  net-plugin 38, clippy clean, rustfmt clean.

- B1g (2026-11-27): review hardening of the B1f carrier
  change. Single-owned the exit-requested predicate:
  `KEY_EVAL_EXIT_REQUESTED` sits beside `FIELD_CODE` in
  `diagnostics.rs`, and `eval_exit_requested_code` owns the
  key-plus-field read; both walkers in `exit.rs` delegate, so
  no literals remain in production code. CLI regression test
  now covers the keyed carrier (`eval_exit_requested(3)` bare
  and under context), proving `process_exit_code` without
  manual `ExitRequest` construction. Full gates: core
  423 + 629 + 12 + 2, diagnostics 8, cli 50 + integration 12,
  mcp 11 + 26 + 13, net-plugin 38, logic-tests all green
  (5 + 2 + 8 + 17 + 15 + 19 + 1 + 4 + 4 + 31 + 6 + 12 + 2 +
  19 + 11), clippy clean, rustfmt clean.

- B1h (2026-11-27): `cargo test --workspace` green, exit 0,
  zero `FAILED`, zero failed counts across every target. The
  two `error: unexpected argument
  '--invalid-oxdock-flag-xyz'` lines in filtered output are
  expected fixture stderr (`run_failure_reporting`
  buildtime-macro fixture plus `oxdock-core` commands
  `run_failure_reporting_*` tests), not failures.

- B1i (2026-11-27): `step_error` span sync. The position wrap
  now attaches `Span` natively via `with_span` on the reclaimed
  diagnostic (innermost wins: span-less only, `line_col` when
  `col > 0`, else `line_only`), while the `step N[,
  line L[, col C]]` text prefix stays byte-identical. Two paths:
  in-place `downcast_mut` fast path when the diagnostic is the
  outermost head, plus a wrapped-chain reclamation slow path
  (chain walk, outer non-request frames replayed as text, exit
  code re-attached typed, sources surviving the clone) for
  synthetic-inner/parsed-outer nesting, AWAIT re-emit frames,
  and helper-added contexts. Outer dispatch levels keep
  text-only chaining. Stale "until flips" comments updated;
  B1 Decision block amended (B1i amendment: the
  `BackgroundHandle` flip additionally requires
  Diagnostic-native frame storage; the "waiting only on the
  span sync" premise is corrected). New `step_error_tests`
  (6 tests) pin native attach, innermost-wins nesting, the bare
  prefix at zero coordinates, prespanned-head preservation,
  synthetic-inner/parsed-outer reclaim, and line-only spans.
  Gates on the actual diff: core 429 + 629 + 12 + 2,
  diagnostics 8, cli 50 + integration 12, mcp 11 + 26 + 13,
  net-plugin 38 + 38, logic-tests all green
  (5 + 2 + 8 + 17 + 15 + 19 + 1 + 4 + 4 + 31 + 6 + 12 + 2 +
  19 + 11 + 4 + 1 + 5 + 3 + 15),
  `clippy --workspace --all-targets` clean,
  `rustfmt --edition 2024 --check` clean.

- B1j (2026-11-27): snapshot provenance + Diagnostic-native
  frame storage. `Diagnostic` gains `contexts: Vec<String>`
  (outermost first; unwinding prepends later outer frames) and
  `snapshot: Option<String>`; `message()` stays pure. `Display`
  renders a `: `-joined prefix without a snapshot, and the
  failure-then-section chain rule with one (1-node plain,
  2-node `first (second)`, 3+-node `first (second)` plus a
  `causes:` block), matching `error_with_snapshot`. Keyed
  failures keep their own diagnostic as the head error with
  `with_snapshot` provenance; `dsl.eval.snapshot_context`
  (`eval_run_snapshot_context(failure, snapshot)`) is reserved
  for unkeyed fallbacks. `error_with_snapshot` formats
  `primary`/`rest`/`failure` only in the unkeyed branch. Gates
  (post-PASS execution): `cargo test --workspace` green (core
  432 + 629 + 12 + 2, diagnostics 12, cli 50 + 12, mcp
  11 + 26 + 13, net-plugin 38 + 38, logic-tests
  5 + 2 + 8 + 17 + 15 + 19 + 1 + 4 + 4 + 31 + 6 + 12 + 2 +
  19 + 11 + 4 + 1 + 5 + 3 + 15), `cargo clippy --workspace
  --all-targets` clean, `rustfmt --edition 2024 --check` clean
  on touched files.

- B1k (2026-11-27): `BackgroundHandle` -> `DiagnosticResult`
  flip. Trait methods (`try_wait`, `kill`, `wait` in
  `contract.rs`) plus all implementors (`ChildHandle`,
  `MockHandle`, miri `SyntheticBgHandle`, core
  `ThreadJoinHandle`, test `BlockingHandle`, logic-tests
  `RecordingHandle` with a new `oxdock-diagnostics` dep) return
  keyed diagnostics. `ThreadJoinHandle::reemit_error` returns
  `Diagnostic` directly with `thread_context` replayed via
  native `with_contexts` (no anyhow layers);
  `step_error`'s slow path replays outer frames the same way
  (`reclaimed.with_contexts(outer)`), keeping the `step N`
  text prefix byte-identical. Core poll sites bridge back with
  `.into()` / `anyhow::Error::new(diag)` at the unflipped
  `anyhow` boundaries; `shell_manager` wraps the wait failure
  for `run_wait_failed`. Child wait failures return
  `wait_failed` directly. Thread tests assert
  `exit_code_of_diag(&diag)` alongside `exit_code_of(&err)`.
  Gates (post-PASS execution): `cargo test --workspace` green
  (core 432 + 629 + 12 + 2, diagnostics 12, process 121, cli
  50 + 12, mcp 11 + 26 + 13, net-plugin 38 + 38, logic-tests
  5 + 2 + 8 + 17 + 15 + 19 + 1 + 4 + 4 + 31 + 6 + 12 + 2 +
  19 + 11 + 4 + 1 + 5 + 3 + 15), `cargo clippy --workspace
  --all-targets` clean, `rustfmt --edition 2024 --check` clean
  on touched files.

- B7 (2026-11-27): closure cascade (`write_stdout`/`write_stderr`
  in `core/exec/io.rs` plus all 10 inline closures in
  `handlers.rs`/`remote.rs`). Both helpers return
  `DiagnosticResult`; the `Os` arms return keyed
  `eval_write_live_pipe` directly, and closures propagate
  natively. Already-mapped sites (`read`, `expand`) drop the
  `anyhow::Error::new` wrapper around identical ctors. Bare
  writer ops (`echo`, `fail_message`, assert echo-backs, `ls`
  header, remote replay + flushes) map io failures through
  the new io-native `eval_stream_write_failed`
  (`dsl.eval.stream_write_failed`, `FIELD_REASON`, message is
  the failure verbatim), keeping the `step N: <reason>`
  rendering byte-identical with zero new `anyhow::Error::new`
  sites. No new keys beyond the one ctor. Gates (post-PASS
  execution): `cargo test --workspace` green (core
  432 + 629 + 12 + 2, diagnostics 12, process 121, cli
  50 + 12, mcp 11 + 26 + 13, net-plugin 38 + 38, logic-tests
  5 + 2 + 8 + 17 + 15 + 19 + 1 + 4 + 4 + 31 + 6 + 12 + 2 +
  19 + 11 + 4 + 1 + 5 + 3 + 15), `cargo clippy --workspace
  --all-targets` clean, `rustfmt --edition 2024 --check` clean
  on touched files.

- B5 (2026-11-27): `ForwardError` redesign.
  `chase_const_forward`/`chase_from` (`core/exec/native.rs`)
  return `DiagnosticResult`; all 9 failure sites build the
  `dsl.import.const_forward_failed` diagnostic through the
  single `forward_failed` conversion owner, with `ForwardError`
  `Display` kept as the sole wording owner (byte-identical
  messages by construction). The `impl std::error::Error`
  (which existed only to feed `anyhow::Error::new` at callers)
  is removed. `import_const_forward_failed` takes `&str`
  (pre-rendered text) and `check_ctx_wrap` takes `&Diagnostic`
  (joins via `{reason:#}`, same flattening as before), so all
  3 call sites (`import.rs` `resolve_const_ref`, two
  `static_check.rs` sites, all already `DiagnosticResult`)
  propagate with no `anyhow` wrap: three
  `anyhow::Error::new` sites eliminated. 4 pin tests updated
  to the new signatures; keys/messages unchanged. Gates:
  `cargo test --workspace` green (core 432 + 629 + 12 + 2,
  diagnostics 12, process 121, cli 50 + 12, mcp
  11 + 26 + 13, net-plugin 38 + 38, logic-tests
  5 + 2 + 8 + 17 + 15 + 19 + 1 + 4 + 4 + 31 + 6 + 12 + 2 +
  19 + 11 + 4 + 1 + 5 + 3 + 15), `cargo clippy --workspace
  --all-targets` clean, `rustfmt --edition 2024 --check` clean
  on touched files.

- H1 (2026-11-27): archive cluster. `resolve_archive_src`,
  `collect_archive_tree`, `append_archive_tree`, `archive`,
  `unarchive` (`core/exec/handlers.rs`) return
  `DiagnosticResult`; 34 outer `anyhow::Error::new(diag::...)`
  wrappers dropped (16 `eval_archive_failed`, 5
  `eval_unarchive_failed`, plus `no_file_name`,
  `refuses_symlink(_source)`, `destination_is_source/inside_tree`,
  tar `into_inner`/`flush`/`try_finish`/`append_data`,
  `bad_max_bytes`, `restore_mode_failed`). Ctors keep their
  `&anyhow::Error` params as the conversion owners (fs/io/sanitize
  errors bridge in; messages byte-identical). 4 anyhow-parent
  boundaries use `Ok(f(...)?)` via blanket `From<Diagnostic>`
  (`dispatch_archive`, `dispatch_unarchive`, two `steps.rs`
  match arms); dispatch signatures and the `define_pipeline!`
  table untouched. The 15 `member_failed`/`expansion_cap`/
  `refuses_symlink` sites inside the `unpack_auto_from` closure
  stay anyhow: the closure must return anyhow `Result` per the
  `oxdock-utils` signature (external-crate boundary). No new keys;
  no test changes (existing archive pins cover). Gates:
  `cargo test --workspace` green (core 432 + 629 + 12 + 2,
  diagnostics 12, process 121, cli 50 + 12, mcp
  11 + 26 + 13, net-plugin 38 + 38, logic-tests
  5 + 2 + 8 + 17 + 15 + 19 + 1 + 4 + 4 + 31 + 6 + 12 + 2 +
  19 + 11 + 4 + 1 + 5 + 3 + 15), `cargo clippy --workspace
  --all-targets` clean, `rustfmt --edition 2024 --check` clean
  on touched files.

- H2a (2026-11-27): read/write cluster. `read`, `read_line`,
  `write`, `append` (`core/exec/handlers.rs`) return
  `DiagnosticResult`; 25 outer `anyhow::Error::new(diag::...)`
  wrappers dropped. Ctors keep their `&anyhow::Error` /
  `&std::io::Error` params as conversion owners (byte-identical
  messages). Already-native sites pass through untouched
  (`write_stdout` closures, `mutate_var`/`declare_var`,
  io-native stdin/read ctors). The 2 bare `writer.flush()?`
  sites map through the neighboring chunk-failure owners
  (`eval_write_stream_failed`, `eval_append_failed`); the two
  `&anyhow::Error::new(e)` bridges there mirror the adjacent
  `write_all` lines (ctor `&anyhow::Error` params), so no new
  outer wrapper sites. 8 anyhow-parent
  boundaries use `Ok(f(...)?)` (`dispatch_read`,
  `dispatch_read_line`, `dispatch_write`, `dispatch_append`,
  four `steps.rs` arms); pipeline table untouched.
  `append_line` (H2b) out of scope. No test changes. Gates:
  `cargo test --workspace` green (core 432 + 629 + 12 + 2,
  diagnostics 12, process 121, fs 71, cli 50 + 12, mcp
  11 + 26 + 13, net-plugin 38 + 38, logic-tests
  5 + 2 + 8 + 17 + 15 + 19 + 1 + 4 + 4 + 31 + 6 + 12 + 2 +
  19 + 11 + 4 + 1 + 5 + 3 + 15), `cargo clippy --workspace
  --all-targets` clean, `rustfmt --edition 2024 --check` clean
  on touched files. Note: one `cargo test --workspace` run
  during this bulk hit the pre-existing environmental
  `oxdock-fs` pid-reuse flake
  (`lazy_resolver_bare_snapshot_selection_creates_nothing`,
  8205 leaked `oxdock-pending-*` dirs in `$TMPDIR`); unrelated
  crate, passes on retry (71/71), re-run green.

- H2b (2026-11-27): fs-ops cluster. `copy`, `place_file_in_dir`,
  `copy_git`, `symlink`, `link_base_name`, `mkdir`,
  `set_executable`, `ls`, `install`, `push_into`, `append_line`,
  `remove` (`core/exec/handlers.rs`) return `DiagnosticResult`;
  35 outer `anyhow::Error::new(diag::...)` wrappers dropped.
  Ctors keep `&anyhow::Error` params as conversion owners
  (byte-identical messages); `eval_list_append_failed` keeps its
  `Diagnostic` param (`push_into_list` already native). Inner
  `link_base_name` propagates natively via `?`; `place_file_in_dir`
  re-wraps through `eval_copy_failed` with one
  `&anyhow::Error::new(e)` bridge (Diagnostic-to-ctor adapter, same
  as H2a flush pattern). Already-native `write_stdout` closure in
  `ls` passes through untouched. 10 `dispatch_*` anyhow-parent
  boundaries plus 10 `steps.rs` arms use `Ok(f(...)?)`; pipeline
  table untouched. No test changes. Gates:
  `cargo test --workspace` green (core 432 + 629 + 12 + 2,
  diagnostics 12, process 121, fs 71, cli 50 + 12, mcp
  11 + 26 + 13, net-plugin 38 + 38, logic-tests
  5 + 2 + 8 + 17 + 15 + 19 + 1 + 4 + 4 + 31 + 6 + 12 + 2 +
  19 + 11 + 4 + 1 + 5 + 3 + 15), `cargo clippy --workspace
  --all-targets` clean, `rustfmt --edition 2024 --check` clean
  on touched files.

- H3 (2026-11-27): run cluster. `run`, `run_argv`, `run_pty`,
  `resolve_run_exec_argv`, `flatten_exec_value`,
  `drain_stdin_bounded`, `exact_stdout_bytes`, `tty_request`,
  `resolve_io_streams`, `resolve_pipe_handle`, `top_level_flow`
  (`core/exec/handlers.rs`) return `DiagnosticResult`; 41 outer
  `anyhow::Error::new(diag::...)` wrappers dropped. Ctors keep
  `&anyhow::Error` params as conversion owners (byte-identical
  messages). One inner `&anyhow::Error::new(e)` bridge kept in
  `run_pty` (`eval_run_spawn_failed` over the pty-session error,
  same adapter pattern as H2a/H2b). Already-native callees pass
  through untouched (`command_ctx`, `check_spawn_anchors`,
  `BackgroundHandle::wait`/`kill`, `load_var_binding`,
  args/io helpers, `write_stdout` closures). 10 anyhow-parent
  `top_level_flow(...)` call sites (H4/H5 owners: `fail`,
  `for_loop`, `if_then`, `switch_run`, `with_io`,
  `handle_return`, `while_loop`, `assign_capture`, `timeout`,
  `isolated`) plus 2 `dispatch_*` fns and 2 `steps.rs` arms
  (`Run`, `RunExec`) bridge via `Ok(f(...)?)`; pipeline table
  untouched. `with_io` itself stays anyhow and holds the flipped
  helpers via `From`. No test changes. Gates:
  `cargo test --workspace` green (core 432 + 629 + 12 + 2,
  parser 267 + 303, diagnostics 12, process 121, fs 71 + 8 + 4 + 1,
  cli 50 + 12, mcp 11 + 26 + 13, net-plugin 38 + 38, logic-tests
  5 + 2 + 8 + 17 + 15 + 19 + 1 + 4 + 4 + 31 + 6 + 12 + 2 +
  19 + 11 + 4 + 1 + 5 + 3 + 15), `cargo clippy --workspace
  --all-targets` clean, `rustfmt --edition 2024 --check` clean
  on touched files.

  H3 follow-up (2026-11-27, post-PASS review note): magic-string
  purge. New single owner `IoStream::name()` (`parser/ast.rs`)
  for the bare `WITH_IO` vocabulary; 6 `duplicate_stream` sites
  use `binding.stream.name()`, 2 `tty_live_pipe` sites use
  `IoStream::Stdin/Stdout.name()`, `fmt_io` renders through
  `name()` (geometry arms interpolate `name()` plus `=value`). `flatten_exec_value`
  derives `map` from `val.type_name().to_lowercase()` (byte
  identical, `KEYWORD_*` lowercase convention); pin tests
  untouched. Parse-direction arms (`&str` to variant) and prose
  messages quoting source spellings stay: defining occurrences,
  not restatements. `task handle task#N` shape in the handle arm
  stays as-is per review (ctor-documented prose with dynamic id,
  no named owner). Follow-up gates: same `cargo test --workspace`
  run green, `cargo clippy --workspace --all-targets` clean,
  `rustfmt --edition 2024 --check` clean on `ast.rs`,
  `commands.rs`, `handlers.rs`, `steps.rs`.

- H4 (2026-11-27): call/dispatch cluster. `workdir`, `sleep`,
  `with_io_block`, `exit`, `for_loop`, `build_script_entry`,
  `resolve_call_entry`, `gate_call`, `execute_call_entry`,
  `block_value`, `while_loop` (`core/exec/handlers.rs`) return
  `DiagnosticResult`; 22 outer `anyhow::Error::new(diag::...)`
  wrappers dropped (6 in `execute_call_entry` alone). `dispatch_break`/`dispatch_continue` stay
  anyhow: the `define_pipeline!` table requires
  `anyhow::Result` signatures (2 wrappers remain as the
  pipeline-boundary share, like H3 `with_io`). Ctors keep
  `&anyhow::Error`/`Diagnostic` params as conversion owners
  (byte-identical messages). Anyhow interiors hold via the single
  owners: `execute_steps`/`inherit_env` failures lift through the
  existing `args::lift_dispatch_error` (now `pub(crate)`,
  upgraded to lossless chain reclamation mirroring
  `step_error`/`reap`: outer non-request frames replay natively
  via `with_contexts`, the exit code rides typed via
  `ExitRequest::with_source` preserving the prior source only when
  the reclaimed chain lacks one, bare `ExitRequest` codes return
  the keyed `eval_exit_requested` carrier with frames instead of
  flattening into `eval_dispatch_failed`; new test pins frame
  preservation on a keyed diagnostic and code-plus-frame
  preservation on a bare request), `Pure`/`Ctx`/`Both` match arms
  convert through
  `eval_call_failed`, `fail` bridges via `anyhow::Error::from`.
  `for_loop`/`while_loop`/`block_value` drop the
  `.map_err(anyhow::Error::from)` round-trip on `pop_scope`.
  Already-native callees pass through untouched (`resolve_tag`,
  `declare_var`, `check_return_tag`, `optional_zero_value`,
  `check_host_return_value`, `coerce_value`, `evaluate_expr`,
  `is_truthy`). 4 `dispatch_*` anyhow-parent bridges plus 3
  `steps.rs` arms (`Workdir`, `Exit`, `Sleep`) and 2
  `dispatch_flow_step` arms (`For`, `While`) use `Ok(f(...)?)`;
  `dispatch_for_loop`/`dispatch_while_loop` need no change
  (already `Ok(top_level_flow(...)?)`). `args.rs` goes native at
  4 sites: `Expr::Block` and 3 RPN call sites drop
  `lift_dispatch_error` (types align; `Expr::Call` keeps it via
  still-anyhow `call_func_value`). `call_func_value` itself stays
  anyhow and holds the flipped `execute_call_entry` via `From`.
  New test `lift_dispatch_error_preserves_frames_and_exit_codes`
  pins frame and exit-code preservation. Gates (post-PASS):
  `cargo test -p oxdock-core` green (433 + 629 + 12 + 2; the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures), full
  `cargo test --workspace` green with zero failures across all
  suites, `cargo clippy --workspace --all-targets` clean,
  `rustfmt --edition 2024 --check` clean on `handlers.rs`,
  `steps.rs`, `args.rs`.

- H5 (2026-11-27): async cluster. `assign_capture`,
  `dispatch_assign_async`, `resolve_task_entry`,
  `resolve_task_entry_by_id`, `await_task_entry`, `dispatch_await`,
  `dispatch_await_capture`, `dispatch_cancel`, `disarm_deadline`,
  `isolated_inherit_env` (`core/exec/handlers.rs`) plus helpers
  `peel_capture_command`, `pin_async_keepers` return
  `DiagnosticResult`; 23 outer `anyhow::Error::new(diag::...)`
  wrappers dropped (5 more in `await_task_entry` alone).
  `dispatch_async_block` stays anyhow: the `define_pipeline!`
  table requires `anyhow::Result` signatures (its 3 thread-closure
  wrappers remain as the pipeline-boundary share, like H4
  `dispatch_break`/`dispatch_continue`). Both
  `std::thread::spawn` closures stay anyhow: `ThreadJoinHandle::new`
  takes `JoinHandle<anyhow::Result<()>>` (external signature
  boundary, like H1 `unpack_auto_from`; 4 wrappers remain inside
  `dispatch_assign_async`'s closure). Ctors keep their params as
  conversion owners (byte-identical). Anyhow interiors lift through
  the single owner `args::lift_dispatch_error` (lossless per H4
  AMEND): `import_file` (x2), `call_func_value` (x2),
  `execute_single_step_with_generation` in `assign_capture`;
  `timeout` (stays anyhow) lifts `execute_scoped_steps` then holds
  flipped `disarm_deadline` via `anyhow::Error::from`.
  `disarm_deadline`'s `with_context(timeout)` arm goes native:
  fired+ok returns the bare keyed `eval_timeout_deadline`, fired+err
  chains the body via `with_source(Arc::new(err))` so the timeout key
  still wins reclamation with the body preserved. `await_task_entry`
  drops `.into()` on the `try_wait` error (`BackgroundHandle` is
  already `DiagnosticResult`-native); `handle.kill()` passes through.
  `native.rs` `task_poll` (stays anyhow) holds flipped
  `resolve_task_entry_by_id` via `From`, no change needed.
  10 anyhow-parent boundaries via `Ok(f(...)?)`: 5 in
  `handlers.rs` (`dispatch_bind_step` `Async`/`Await` arms,
  `dispatch_await_step`, `dispatch_cancel_step`,
  `dispatch_isolated_inherit_env`), 4 in
  `execute_single_step_with_generation` (`Bind Async`, `Bind Await`,
  `Await`, `Cancel`), 1 in `dispatch_flow_step` (`Bind Capture`).
  `dispatch_async_block` needs no bridge (stays anyhow for the
  `define_pipeline!` table, so the `AsyncBlock` leaf arm passes it
  through untouched); the `IsolatedInheritEnv` leaf arm needs none
  either (already `?; Ok(())`). No test changes. Gates (post-PASS):
  `cargo test -p oxdock-core` green (433 + 629 + 12 + 2; the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures), full
  `cargo test --workspace` green with zero failures across all
  suites, `cargo clippy --workspace --all-targets` clean,
  `rustfmt --edition 2024 --check` clean on `handlers.rs`,
  `steps.rs`.

- H6a (2026-11-27): assert cluster. `assert_equality`,
  `assert_contains` (`core/exec/handlers.rs`) plus
  `dispatch_assert_equality` return `DiagnosticResult`; 35 outer
  `anyhow::Error::new(diag::...)` wrappers dropped (20 + 15).
  Ctors keep their params as conversion owners (byte-identical).
  `dispatch_assert_eq`/`dispatch_assert_ne`/`dispatch_assert_contains`
  stay anyhow: the `define_pipeline!` table requires `anyhow::Result`
  signatures (same as H4 `dispatch_break`/`dispatch_continue`, H5
  `dispatch_async_block`). Already-native callees pass through
  untouched (`drain_stdin_bounded`, `exact_stdout_bytes`,
  `write_stdout`, `resolve_arg`, `evaluate_assert_operand`,
  `resolve_assert_target`). 5 anyhow-parent boundaries via
  `Ok(f(...)?)`: 3 in `handlers.rs` (`dispatch_assert_eq`,
  `dispatch_assert_ne` holding flipped `dispatch_assert_equality`,
  `dispatch_assert_contains` holding flipped `assert_contains`), 2 in
  `execute_single_step_with_generation` (`AssertEq`/`AssertNe` arm,
  `AssertContains` arm). No test changes. Gates (post-PASS):
  `cargo test -p oxdock-core` green (433 + 629 + 12 + 2; the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures), full
  `cargo test --workspace` green with zero failures across all
  suites, `cargo clippy --workspace --all-targets` clean,
  `rustfmt --edition 2024 --check` clean on `handlers.rs`,
  `steps.rs`.

- H6b (2026-11-27): replace/prompt cluster. `replace`, `prompt`,
  `bind_prompt_var` (`core/exec/handlers.rs`) return
  `DiagnosticResult`; 7 outer `anyhow::Error::new(diag::...)`
  wrappers dropped (5 in `prompt`, 2 in `bind_prompt_var`).
  `replace` internals were already native (no outer wrappers to
  drop); its `{{ MODULE::FUNC }}` closure stays anyhow: the
  `PlaceholderCall` alias in `oxdock-process` (`expand.rs`) is an
  external-crate `anyhow::Result` boundary (same as H1
  `unpack_auto_from`, H5 thread closures). The single
  `&anyhow::Error::new(e)` bridge for `eval_expand_read_failed`
  stays as a ctor-argument adapter (the ctor takes
  `&anyhow::Error`; same pattern as H2a/H2b/H3). Already-native
  callees pass through untouched (`command_ctx`, `resolve_read`,
  `open_read`, `expand`/`read`/`stdin` ctors,
  `StreamingExpand::process_bytes`/`flush`, `write_stdout`,
  `mutate_var`/`declare_var` via `eval_prompt_bind_failed`
  taking `Diagnostic` by value). `dispatch_expand`/
  `dispatch_prompt` stay anyhow: the `define_pipeline!` table
  requires `anyhow::Result` signatures (same as H6a
  `dispatch_assert_*`, H5 `dispatch_async_block`, H4
  `dispatch_break`/`dispatch_continue`). 4 anyhow-parent
  boundaries via `Ok(f(...)?)`: 2 in `handlers.rs`
  (`dispatch_prompt`, `dispatch_expand`), 2 in
  `execute_single_step_with_generation` (`Prompt`, `Expand` arms).
  No test changes. Gates: `cargo test -p oxdock-core` green
  (433 + 629 + 12 + 2; the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures), full
  `cargo test --workspace` green with zero failures across all
  suites, `cargo clippy --workspace --all-targets` clean,
  `rustfmt --edition 2024 --check` clean on `handlers.rs`,
  `steps.rs`.

- A2 (2026-11-27): import boundary. `import_funcs`,
  `ensure_module_init`, `import_file` (`core/exec/import.rs`)
  return `DiagnosticResult`; 7 outer
  `anyhow::Error::new(import_diag::...)` wrappers dropped (host
  `import_unknown_type` probe, `import_stateful_harvest`,
  `import_unknown_function`, `import_unknown_type` None,
  `import_type_collision`, plus `import_flow_crosses_boundary`
  BREAK/CONTINUE). `nested_cause` becomes
  `Option<oxdock_diagnostics::Diagnostic>` (recursive calls now
  native, no `anyhow` carrier). `pop_scope` shim dropped
  (`pop_scope` already `DiagnosticResult`; the
  `.map_err(anyhow::Error::from)` round-trip removed, match arms
  native). `execute_steps` interior (still anyhow, B1 keystone)
  lifts via the single owner `args::lift_dispatch_error`
  (lossless per H4 AMEND, same as `execute_steps`/`inherit_env`
  in handlers). Already-native callees pass through untouched
  (`load_import`, `gate_harvested_name`,
  `insert_imported_type`, `resolve_own_type_harvests`,
  `collect_type_aliases`, `validate_import_body_types` via
  `import_failed_validation` taking `Diagnostic` by value,
  `resolve_script_params`, `resolve_tag`, `check_return_tag`,
  `import_cannot_read`/`import_bad_utf8`/`import_parse_failed`
  via `&anyhow::Error`/`&io` ctor params).
  `read_import_bytes` stays anyhow (fs boundary, B4 permanent).
  Anyhow parents hold via `From`: `dispatch_import_file`/
  `dispatch_import_funcs` stay anyhow (`define_pipeline!` table)
  via `Ok(f(...)?)`; `steps.rs` ImportFile/ImportFuncs arms need
  no change (Diagnostic converts via `From` into the anyhow
  `?`); `assign_capture` Import arms (already
  `DiagnosticResult`) drop the `.map_err(lift_dispatch_error)`
  round-trip. No test changes. Gates (post-PASS):
  `cargo test -p oxdock-core` green (433 + 629 + 12 + 2; the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures), full
  `cargo test --workspace` green with zero failures across all
  suites, `cargo clippy --workspace --all-targets` clean,
  `rustfmt --edition 2024 --check` clean on `import.rs`,
  `handlers.rs`.

- A3 (2026-11-27): mcp session init. `McpSession::init`
  (`core/exec/mcp.rs`) returns `DiagnosticResult`; 4 outer
  `anyhow::Error::new(mcp_diag::...)` wrappers dropped
  (`mcp_export_type_unknown`, `mcp_break_outside_loop`,
  `mcp_continue_outside_loop`, `mcp_return_outside_function`).
  `execute_steps` interior (still anyhow, B1 keystone) lifts via
  the single owner `args::lift_dispatch_error`. Already-native
  callees pass through untouched (`new_state`,
  `check_pre_run`, `validate_closure_for_run`,
  `exported_metas`); `use anyhow::Result` removed (no remaining
  `anyhow` site in the file; `std::io::Result` stays fully
  qualified). Anyhow parents hold via `From`: `oxdock-mcp`
  `build_inner` (still anyhow) needs no change; test callers
  unchanged. No test changes. Gates (post-PASS):
  `cargo test -p oxdock-core` green (433 + 629 + 12 + 2; the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures), full
  `cargo test --workspace` green with zero failures across all
  suites, `cargo clippy --workspace --all-targets` clean,
  `rustfmt --edition 2024 --check` clean on `mcp.rs`.

- A4a (2026-11-27): runner flow boundary. `finish_run`
  (`core/exec/mod.rs`) returns `DiagnosticResult`; 3 outer
  `anyhow::Error::new(...)` wrappers dropped
  (`check_flow_outside_loop` BREAK/CONTINUE,
  `check_return_outside`). `execute_steps` interior (still
  anyhow, B1 keystone) lifts via the single owner
  `args::lift_dispatch_error` (same as A2 `ensure_module_init`,
  A3 `McpSession::init`). Already-native callees pass through
  untouched (`new_state`, `check_pre_run`,
  `validate_closure_for_run`). Anyhow parent holds via `From`:
  `run_steps_with_manager_with_modules_and_entry` (still anyhow,
  public API decision open) bridges via `Ok(finish_run(...)?)`;
  no caller changes needed. The public `run_steps_*` family
  plus `error_with_snapshot`/`enrich_lazy_error` Diagnostic-outer
  shape stays open pending the B1 decision. No test changes.
  Gates (post-PASS):
  `cargo test -p oxdock-core` green (433 + 629 + 12 + 2; the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures), full
  `cargo test --workspace` green with zero failures across all
  suites, `cargo clippy --workspace --all-targets` clean,
  `rustfmt --edition 2024 --check` clean on `mod.rs`.

- B1l (2026-11-27): `execute_steps` keystone flip.
  `execute_steps`, `execute_steps_inner`,
  `execute_single_step_with_generation`, `execute_scoped_steps`,
  `execute_scoped_remote_body`, `dispatch_flow_step`
  (`core/exec/steps.rs`) return `DiagnosticResult`; `step_error`
  takes/returns `Diagnostic` natively (`mut err`, `with_span`
  innermost-wins plus `with_contexts` prepend, byte-identical
  `step N` prefix, no anyhow chain walk). Reaper/poll goes
  native (`failed_status: Option<Diagnostic>`,
  `Poll::CompletedErr(Diagnostic)`, no `.into()`).
  `dispatch_flow_step` lifts anyhow callees (`define_func`,
  `call_func_value`, `handle_return`, `if_then`, `switch_run`,
  `timeout`, `fail`, `isolated`, `with_io`) via the single owner
  `args::lift_dispatch_error`; native callees pass through.
  `execute_single_step` lifts anyhow leaves (`echo`,
  `dispatch_async_block`, `workspace`, `run_remote_block`, `env`,
  `inherit_env`, `copy_transfer`, `assign`, `set_var_value`).
  Callers drop obsolete lifts (`for_loop` x2, `while_loop`,
  `execute_call_entry`, `block_value`, `assign_capture`,
  `import.rs`, `mcp.rs`, `mod.rs` `finish_run` now native `?`).
  Anyhow parents hold via `From` (`with_io`, `if_then` x3 via
  `Ok(...?)`, `run_remote_block`, `timeout`, `isolated` via native
  `with_contexts`). Redundant `Ok(handler(...)?)` bridges stripped
  across `steps.rs` so each arm returns the `DiagnosticResult`
  directly. `step_error_tests` rewritten Diagnostic-native. No new
  keys. Gates:
  `cargo test -p oxdock-core` green (433 + 629 + 12 + 2; the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures), full
  `cargo test --workspace` green with zero failures across all
  suites, `cargo clippy --workspace --all-targets` clean,
  `rustfmt --edition 2024 --check` clean on touched files
  (`steps.rs`, `handlers.rs`, `mod.rs`, `import.rs`, `mcp.rs`,
  `remote.rs`).

- A6 (2026-11-27): mcp server boundary. `reload`, `session_fs`,
  `read_guarded`, `build_inner` (`oxdock-mcp/server.rs`) return
  `DiagnosticResult`; 5 outer `anyhow::Error::new(...)` wrappers
  dropped (`session_lock_poisoned_reload`,
  `session_guard_workspace_root`, `session_guard_script_root`,
  `export_no_entries`, `export_status_name_reserved`). Two
  previously-bare anyhow leaves gain keyed owners (no magic
  strings, no info loss: reason chain renders inline):
  `session_script_parse_failed`
  (`mcp.session.script_parse_failed`) for
  `parse_script_with_modules_and_exports`,
  `session_script_read_failed`
  (`mcp.session.script_read_failed`) for `read_to_string`.
  Already-native callees pass through untouched
  (`McpSession::init`, `tool_metas`, `reject_reserved_metas`,
  `build_tool_table`). Anyhow parents hold via `From` with no
  caller changes (`init_handler`, `from_built`, `new`,
  `from_parts`, `spawn_reloader` match arms, tests). Pin tests
  extended (distinct keys + canonical messages for both new
  ctors). Reload-failure `message.contains("BROKEN")` contract
  preserved via inline reason rendering. Gates:
  `cargo test -p oxdock-mcp` green (11 + 13 + 26 + 0), full
  `cargo test --workspace` green with zero failures across all
  suites (the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures),
  `cargo clippy --workspace --all-targets` clean (zero warnings),
  `rustfmt --edition 2024 --check` clean on touched files
  (`server.rs`, `diagnostics.rs`).
