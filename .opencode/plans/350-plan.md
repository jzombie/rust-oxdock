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

- [ ] H1 archive cluster (58): `collect_archive_tree`,
      `append_archive_tree`, `archive`, `resolve_archive_src`,
      `unarchive`
- [ ] H2a read/write cluster (30): `write`, `append`, `read`,
      `read_line`
- [ ] H2b fs-ops cluster (35): `symlink`, `remove`, `mkdir`,
      `set_executable`, `ls`, `install`, `append_line`,
      `push_into`, `copy`, `copy_git`, `place_file_in_dir`,
      `link_base_name`
- [ ] H3 run cluster (41): `run`, `run_argv`, `run_pty`,
      `resolve_run_exec_argv`, `flatten_exec_value`,
      `resolve_io_streams`, `top_level_flow`,
      `drain_stdin_bounded`, `exact_stdout_bytes`,
      `tty_request`, `resolve_pipe_handle`
- [ ] H4 call/dispatch cluster (29): `execute_call_entry`,
      `resolve_call_entry`, `gate_call`, `block_value`,
      `build_script_entry`, `for_loop`, `while_loop`,
      `dispatch_break`, `dispatch_continue`, `with_io_block`,
      `exit`, `workdir`, `sleep`
- [ ] H5 async cluster (29): `assign_capture`,
      `dispatch_async_block`, `dispatch_assign_async`,
      `resolve_task_entry`, `resolve_task_entry_by_id`,
      `dispatch_await`, `dispatch_await_capture`,
      `await_task_entry`, `dispatch_cancel`, `disarm_deadline`,
      `isolated_inherit_env`
- [ ] H6a assert cluster (35): `assert_equality`,
      `assert_contains`
- [ ] H6b replace/prompt cluster (18): `replace`, `prompt`,
      `bind_prompt_var`

### A2. `core/exec/import.rs` boundary returns (9 sites)
- [ ] Flip with B1: `import_unknown_function`,
      `import_stateful_harvest`, `import_type_collision`,
      `import_flow_crosses_boundary` x2, `import_unknown_type`
      host-module probe, `nested_cause`
- [ ] `pop_scope` `anyhow::Error::from` shim (standard interop;
      drops when B1 flips)
- [ ] `import_const_forward_failed` (needs B5, not B1)

### A3. `core/exec/mcp.rs` (4 sites)
- [ ] Flip with B1: `mcp_export_type_unknown`,
      break/continue/return-outside

### A4. `core/exec/mod.rs` public runner API
- [ ] `run_steps_with_context*` family + BREAK/CONTINUE/
      RETURN-outside returns. Needs B1 decision (public API
      affects CLI/mcp/doc-tests callers), then mechanical.

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
- [ ] Flip with B1: `session_lock_poisoned_reload`,
      guard-workspace/script-root, `export_no_entries`,
      status-name-reserved

## B. Decisions (record outcome on the Decision line)

- [ ] B1 `execute_steps` keystone. Settle together: `step_error`
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
      `error_with_snapshot` gains an outer `dsl.eval.snapshot_context`
      ctor with the snapshot as message plus `with_source` chain,
      preserving `ExitRequest` via `exit_code_of_diag`. `ThreadJoinHandle`
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
- [ ] B5 `ForwardError` redesign (small, self-contained, no
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
- [ ] B7 Closure cascade: `write_stdout`/`write_stderr`
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
  `clippy --workspace --all-targets` clean,
  `rustfmt --edition 2024 --check` clean. (Dependent crates
  cli/mcp/net-plugin/diagnostics/logic-tests gates from the
  pre-reclaim tree; re-gated after review sign-off.)
