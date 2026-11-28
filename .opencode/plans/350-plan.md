# Remaining work: diagnostics unification (#168)

Goal: every user-facing DSL/CLI error behind one keyed constructor;
`anyhow` only as transparent transport where a foreign/macro/thread
boundary forces it, never as a construction site. No permanent
stays for `anyhow`-constructed errors. Changes stay uncommitted for
review until the user explicitly asks for a commit.

Compliance rule (enforced, no bypass): production code MUST NOT
construct user-facing errors via `anyhow::Error::new`, `anyhow::bail!`,
`::anyhow::bail!`, `.context`/`.with_context`, or bare literals.
Every failure site builds its keyed `Diagnostic` through a single
conversion owner (ctor taking a typed source, or a `from_anyhow` /
`lift_dispatch_error`-style lossless lift that reclaims the first
keyed `Diagnostic` in the chain and replays outer frames via native
`with_contexts`). `anyhow` survives ONLY as the transport type at a
named foreign/macro/thread boundary (`From<Diagnostic>`,
`anyhow::Error::from(diag)`, `Ok(f(...)?)` bridges), with a keyed
owner on at least one side. `&anyhow::Error` ctor params are NOT
compliant: replace with typed-source ctors or `from_anyhow`-style
lifts. Magic strings are never acceptable.

Legend: `- [ ]` open, `- [x]` done, `- [!]` stay (closed as permanent).
Stays are BANNED for `anyhow`-constructed user-facing errors under
the compliance rule above: no new `- [!]` may be added for a
construction site, and all prior `- [!]` construction closes are
revoked to `- [ ]` until converted.
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
- [x] `run_steps_with_context*` family. B1 decision applied
      (public API flips to `DiagnosticResult`; CLI/mcp/doc-tests
      callers hold via `From`, `finish_run` native `?` with no
      caller changes; `error_with_snapshot`/
      `enrich_lazy_error` Diagnostic-outer), then mechanical.

### A5. `oxdock-build` fn flips (dep stays; see below)
- [x] Flip remaining fns to `DiagnosticResult`, dropping the
      `anyhow::Error::new(ctor(...))` construction sites
      (`assets.rs` 18 sites already use foreign-error ctors;
      `lib.rs` already anyhow-free)
- Note: the `anyhow` dep CANNOT yet be dropped: the
  `build.*` ctors take `reason: &anyhow::Error`
  (`diagnostics.rs:26-241`), and that non-compliant surface
  converts per Section C/D (typed sources or `from_anyhow`
  lifts once `oxdock-embed` converts). Any `&anyhow::Error`
  in a signature requires `anyhow` in `[dependencies]`;
  removal would fail `cargo check` with unresolved crate.
  `oxdock-build` therefore stays in Section C's interim
  carrier list, open until zero construction sites remain.

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
- [x] B2 `ProcessManager` trait (`contract.rs`,
      `shell_manager.rs`, `mock.rs`, `synthetic.rs`,
      `child.rs`). Flip trait to `DiagnosticResult` (the
      anyhow-at-the-sys-boundary permanent option is revoked
      under the compliance rule).
      Decision: Flip approved. Every failure mode on the run/spawn
      surface is already keyed (`run_command_failed`,
      `run_bg_capture_unsupported`, miri `*_unsupported_miri`,
      `run_exec_requires_argv`, lock-poison pair); the generic
      lifts already exist (`from_anyhow` reclaims keyed payloads
      and names the rest `synthetic_failed`, used by `builder.rs`
      the same way); the rest of the surface already returned
      `DiagnosticResult` (`BackgroundHandle` per B1k,
      `CommandBuilder`, `apply_ctx`, `direct_cmd`,
      `spawn_child_with_streams`); only 2 core call sites
      (`handlers.rs` spawn arms) bridge via `anyhow::Error::from`.
      `child.rs` converts per reopened B9 (no accept-as-is)
      and `expand.rs` `PlaceholderCall` gains a keyed owner or
      lossless lift at the external-crate boundary per the
      compliance rule, so the `anyhow` dep drops when zero
      `anyhow::` construction sites remain: Section C tracks
      `oxdock-process` open until then.
- [ ] B3 `RemoteRunner` trait + `TransferStage`
      (`net session.rs` 36 sites, `core/remote.rs`
      `run_remote_block`). Redesign REQUIRED under the
      compliance rule: close-as-permanent is revoked.
      Concrete-error params that flatten chained
      `RpcServiceError`/`FrameEncodeError` io are NOT an
      excuse to keep `anyhow` construction: introduce the
      typed-error redesign (keyed ctors with typed sources +
      lossless `from_anyhow`-style lift, same shape as H1/H5/H6b
      and A5/B6) so every site builds a keyed `Diagnostic`.
      Decision: REOPENED. No permanent stay; redesign then
      convert, with reclaim + frame-preservation pins.
- [ ] B4 `oxdock-fs` conversion (~200 sites: `io.rs` 68,
      `backend.rs` 37, `copy.rs` 27, `git/` 27, `install.rs`
      12, `path.rs` 10, ...). Permanent exclusion is REVOKED
      under the compliance rule (affirms-#345 close is
      rescinded for this plan). Required path: (b) move shared
      keyword constants to a leaf crate to break the parser
      cycle, then standard bulk conversion to keyed ctors +
      `DiagnosticResult`, file-sized bulks with the A1 pattern.
      No literals ship; no `anyhow` construction remains.
      Decision: REOPENED. Convert; exclusion is not an option.
- [x] B5 `ForwardError` redesign (small, self-contained, no
      input needed): `chase_const_forward` ->
      `DiagnosticResult`; unblocks `import_const_forward_failed`
      + `check_ctx_wrap` sites (`import.rs`, `static_check.rs`
      :1022,:5166).
      Decision: Approved. Small bulk, runs after B1 with no
      further input.
- [x] B6 `oxdock-utils/tar.rs` pack/unpack (3 sites): needs io
      ctors + `apply`-callback migration design first.
      Decision: Deferred design → executed. Two io owners
      (`tar.pack.failed`, `tar.unpack.failed`, verbatim like
      `build.operation.failed`) plus lossless `from_anyhow`
      (mirroring `lift_dispatch_error`); the `apply` callback
      stays `anyhow::Result` (callers span interim-carrier
      crates `cli` + net-plugin), lifted via `from_anyhow`, the
      sanctioned external-boundary shape (H1/H5/H6b).
- [x] B7 Closure cascade: `write_stdout`/`write_stderr`
      (`core/io.rs:368,392`) + 10+ inline closures in
      `handlers.rs`/`remote.rs`. Flip as one cascade with
      callers, inside B1's blast radius.
      Decision: Approved as a B1 cascade. Flips together with
      callers inside B1, not as a separate bulk.

## C. Per-crate closure (grep `anyhow::` paths -> keyed owner or
lossless lift -> remove dep -> gates; `&anyhow::Error` ctor params
do NOT count as closure)

- [x] `oxdock-diagnostics` (anyhow dev-only already)
- [ ] `oxdock-pty` (REOPENED: 4 `&anyhow::Error`
      ctor params over portable-pty third-party failures are
      non-compliant construction sites; replace with typed-source
      ctors or a `from_anyhow`-style lossless lift, same shape
      as A5/B6; dep drops only when zero `anyhow::` paths remain
      in src)
- [ ] `oxdock-process` (REOPENED: B2 flipped the trait but
      `from_anyhow` owners stay while `&anyhow::Error` ctor
      params/bridges, the external `expand.rs`
      `PlaceholderCall` boundary, and B9 `cfg(windows)`
      `child.rs` remain; each must gain a keyed owner or
      lossless lift; dep drops only at zero `anyhow::` paths)
- [x] `oxdock-pipe` (zero `anyhow` paths in src, no dep;
      trivially clean)
- [ ] `oxdock-build` (REOPENED: `build.*` ctors in
      `diagnostics.rs:26-241` take `reason: &anyhow::Error`
      (18 sites); replace with typed-source ctors or
      `from_anyhow`-style lossless lifts once `embed`
      converts per D; dep drops only at zero `anyhow::`
      paths)
- [ ] `oxdock-utils` (REOPENED: `pack_failed` /
      `unpack_failed` take `&anyhow::Error`, `tar.rs` uses
      `anyhow::Error::new(e)` ctor-arg adapters plus the
      `apply` `anyhow::Result` callback boundary; replace
      ctors with typed sources or `from_anyhow`-style lifts,
      lift the callback losslessly; dep drops only at zero
      `anyhow::` construction sites)
- [ ] Interim carriers, NOT permanent (anyhow survives ONLY as
      the transport type with a keyed owner on at least one
      side; every crate below must reach zero `anyhow::`
      construction sites, then drop the dep): `core`, `parser`,
      `cli`, `mcp`, all three plugins, `oxdock-build` (per
      bullet above), `oxdock-utils` (per bullet above),
      `oxdock-pty`, `oxdock-process`, `fs` (per reopened B4),
      `embed` (per reopened D item below)

## D. Reopened conversions (no permanent stays)

Compliance note: NO user-facing `anyhow` construction site may
be recorded here as final. Every item below is either (a) a
pure-transport read with a keyed `Diagnostic` owner on at least
one side, or (b) REOPENED as required conversion work. The
prior "permanent / accept-as-is / won't-fix" closes are revoked.

- [ ] `exit_code_of(&anyhow::Error)` (REOPENED: migrate every
      call site to `exit_code_of_diag(&Diagnostic)`; the anyhow
      carrier read survives ONLY until the last transport is
      keyed, then the `&anyhow::Error` walker is deleted)
- [ ] `check_value_at` / `tag.rs` generated extractors
      (REOPENED: normalize the 4 markdown
      `toc_option_check_failed` sites to keyed passthroughs; no
      won't-fix. The FOLLOW-UP at
      `diagnostics-unification.md:2957` is tracked here until
      that file exists in this repo, then closed there too)
- [ ] `#[oxdock_func]` fns everywhere + func-macro
      `::anyhow::bail!` codegen (REOPENED: macro must emit
      keyed `Diagnostic` construction + `DiagnosticResult`
      signatures; the 11 codegen sites in `oxdock-func-macro`
      convert, no `bail!` remains)
- [ ] `russh Handler::Error = anyhow::Error`
      (`ssh auth.rs:93`/`runtime.rs:253`; REOPENED: assoc type
      stays ONLY because the foreign trait names it, but every
      internal failure builds a keyed `Diagnostic` lifted via
      `From`/`from_anyhow` at the boundary; zero
      `anyhow::Error::new`/`bail!` construction inside)
- [ ] `macro_input.rs` proc-macro boundary (REOPENED:
      `anyhow::Result` over the syn/`TokenStream` surface in
      `parser/src/macro_input.rs` converts to keyed ctors +
      lossless lift for `syn::Error`; `anyhow` remains ONLY as
      the transport type where the proc-macro signature forces
      it)
- [ ] `oxdock-embed` gather fns (REOPENED: `use
      anyhow::{Context, Result}` + `.with_context` chains
      convert to `DiagnosticResult` with keyed ctors; callers
      keep bridging via build ctors only until then)
- [ ] `PumpDownstreamClosed` sentinel + pump thread fns
      (REOPENED: sentinel becomes a keyed `Diagnostic`
      (`std_pump_*` family); thread fns return
      `DiagnosticResult` or lift losslessly at the spawn
      boundary; no `anyhow::Error::new` sentinel, no downcast
      guards)
- [ ] `eval_int_arg_resolve` + `std_pipe_{read,create}_failed`
      (REOPENED: replace the `&anyhow::Error::new(err)`
      ctor-arg adapters with typed-source ctors or
      `from_anyhow`-style lifts; inners already reach sourced
      `os_pipe_creation_failed` in `pipe/slot.rs`, so keep
      that sourcing while deleting the anyhow adapter)
- [ ] CLI help/version `bail!` (REOPENED: user-facing CLI
      errors get keyed owners like every other CLI path;
      `lib.rs` `usage()`/`version()` paths convert)
- [ ] Out of scope, transport-only: test-only crates
      (test-harness / logic-tests / fixture / doc-tests) may
      hold flipped callees via `From`, but MUST NOT construct
      user-facing errors via `anyhow::Error::new`/`bail!`;
      keyed pins stay byte-identical. Dev tooling (`xtask`,
      render tooling) is exempt from the production
      `DiagnosticResult` conversion and dep-removal rule, but
      MUST NOT introduce new user-facing `anyhow`
      construction sites.
- [ ] B9 Windows-only code (`process/child.rs` job objects,
      `cli` re-exec x5, `assign_job` + unused
      `job_assign_failed` ctor): REOPENED, not accept-as-is.
      Convert to `DiagnosticResult` with the keyed
      `job_assign_failed` owner; `cfg(windows)`-gated tests pin
      it; Windows CI verifies. No platform exemption from the
      compliance rule.
      (B3/B4 tracked solely in Section B above, not duplicated
      here.)

## E. Suggested order

1. Decide B1 -> unlocks A1-A4, A6, A2. (done)
2. B4 conversion: keyword-constant leaf move, then file-sized
   fs bulks to keyed ctors (no exclusion).
3. B2/B3/B9 end-state: process + net session + windows code to
   `DiagnosticResult` with lossless lifts (no accept-as-is).
4. Run bulks (H1-H6b file-sized), B5/B6 designs, B7 inside B1.
   (done)
5. Section C per-crate closure: zero `anyhow::` construction
   sites per crate, then drop the dep. `&anyhow::Error` ctor
   params do not count.
6. Section D converted, not recorded: every item above becomes
   `[x]` by conversion; future audit reads zero `anyhow::`
   construction sites in src, zero open items.

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

- B2 (2026-11-27): `ProcessManager` trait flip.
  `run_command`/`spawn_command`/`run_argv`/`spawn_argv`
  (`contract.rs`) return
  `DiagnosticResult<CommandResult<Handle>>`; the `run_argv`
  default body drops its wrapper (`run_argv_unimplemented`
  directly). Implementors flip with it: `ShellProcessManager`
  (`run_command`, `run_argv`, plus `run_prepared`; wait arm
  bridges the keyed `Diagnostic` via `anyhow::Error::from`
  into `run_wait_failed`, byte-identical rendering),
  `MockProcessManager` (`run_command`, `run_argv`, plus
  `capture_stdin`), `SyntheticProcessManager` (miri-only;
  helpers `execute_sync`/`plan_background`/`execute_argv_sync`/
  `plan_argv_background`/`resolve_write` stay anyhow, mapped at
  the two trait fns via the single owner
  `proc_diag::from_anyhow`, the same lift `builder.rs` uses).
  One new keyed ctor for the single bare site on the flipped
  path: `run_stdin_read_failed` (`proc.run.stdin_read_failed`,
  typed `std::io::Error` source like pty `master_read_failed`;
  distinct from lock-poison `run_stdin_lock_failed`), with
  distinct-key + message pins. Core holds via the A6 bridge
  shape: both `handlers.rs` spawn arms map with
  `&anyhow::Error::from(e)` into the `&anyhow::Error` eval
  ctors (unchanged, byte-identical `{reason:#}` inline).
  Test doubles flip with no assertion changes:
  `RecordingManager`, `TtyManager` (delegation stays direct),
  `BlockingRunner` (pure `Ok`), `FailingRunner`
  (`bail!("simulated failure")` becomes keyed
  `eval_dispatch_failed`, message verbatim; unused
  `use anyhow::bail` dropped). `child.rs` untouched (B9),
  `expand.rs` alias untouched (external boundary). anyhow
  parents need no change (test `.expect(...)` call sites hold
  via `Debug`). Gates: `cargo test -p oxdock-process` green
  (121 + 0), `cargo test -p oxdock-core` green
  (433 + 629 + 12 + 2; the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures), full
  `cargo test --workspace`: first run green everywhere except
  the known pre-existing environmental `oxdock-fs` pid-reuse
  flake (`lazy_resolver_bare_snapshot_selection_creates_nothing`;
  zero B2 files touch `oxdock-fs`, suite passes 71/71 on
  retry per the H2a precedent); clean re-run green with zero
  failures across all suites.
  `cargo clippy --workspace --all-targets` clean (zero
  warnings), `rustfmt --edition 2024 --check` clean on all 9
  touched files.

- A4 (2026-11-27): public runner family. All 13 public fns
  (`core/exec/mod.rs`: `run_steps`,
  `run_steps_without_snapshot`, `run_steps_with_context`,
  `run_steps_with_context_result`,
  `run_steps_with_context_result_with_io`,
  `run_steps_with_lazy_snapshot`(+`_and_modules`,
  +`_and_modules_and_entry`), `run_steps_with_fs`,
  `run_steps_with_fs_with_io`, `run_steps_with_manager`
  (+`_with_modules`, +`_with_modules_and_entry`)) return
  `DiagnosticResult`; `run_steps_inner`,
  `error_with_snapshot`, `enrich_lazy_error` Diagnostic-native.
  `PathResolver::{new,new_guarded,new_lazy}` fs-boundary
  failures lift via the single owner
  `args::lift_dispatch_error` (B4 permanent; keyed or
  `dispatch_failed` fallback, messages byte-identical).
  `error_with_snapshot` collapses to `diag.with_snapshot`
  (inputs are always keyed post-B1l; exit codes and call-path
  frames already ride natively). Redundant `Ok(finish_run(...)?)`
  unwrapped. Anyhow parents hold via `From` with no logic
  changes: `Engine::run_steps`, CLI `run_cli_steps`/
  `run_script` (`Ok(...?)`), build `assets.rs` (`&anyhow::from`
  bridge for the `&anyhow::Error` ctor param, permanent per A5
  note), 9 core integration `run_script` helpers plus
  `commands.rs` x3 / `remote.rs` x2 (`.map_err(anyhow::from)`
  so `exit_code_of`/downcast pins ride the same node),
  logic-tests `text_surgery`/`multiline`/`perf` helpers plus
  `release run_validate`, `recording`/`ast_runner` untouched
  (`expect`/`?`/`.map_err(enrich_lazy_error)` align),
  `conformance.rs`/`docs_conformance` (`format!("{err:#}")`
  identical: `Display` ignores alternate), macros `map_err`
  (`{e:#}` identical, no edit). Test-harness helpers widen to
  `&dyn Display` (`format_fixture_stderr`,
  `assert_error_matches`); `run_expect_err` keeps its anyhow
  return via `.map_err(anyhow::from)` so all downstream pins
  hold. Three `snapshot_outer_*` tests rewritten
  Diagnostic-native (keyed exit + frames + snapshot pin,
  bare-request carrier pin, sourceless pin);
  `eval_run_snapshot_context` stays reserved (B1j, `pub` API).
  `FailingRunner` mock qualifies `anyhow::Result` (glob no
  longer re-exports it). Follow-up within the bulk:
  `conformance.rs run_example` formats via
  `anyhow::Error::from(err)` (`{:#}`): the bare `Diagnostic`
  Display holds message plus span only while doc needles
  (`PARSE_ARGS` reasons) live in the source chain, so the
  carrier wrap restores the exact pre-flip chain text
  (`returns_gate_covers_std_builtins` caught it, passes
  unmodified). Second follow-up in the same shape:
  `install_conformance missing_asset_fails_naming_the_file`
  probes a bare-manager error whose file-name needle lives in
  the source chain, so it bridges with
  `.map_err(anyhow::Error::from)` before `expect_err`
  (negative assertions like `release` TBD-gate stay unwrapped).
  Third follow-up, same shape: `release_script_conformance
  missing_section_fails_naming_the_needle` probes a bare-manager
  error whose `[9.9.9-t]` needle lives in the source chain, so it
  bridges the same way.
  Gates: `cargo test -p oxdock-core` green
  (433 + 629 + 12 + 2; the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures), full
  `cargo test --workspace` green with zero failures across all
  suites, `cargo clippy --workspace --all-targets` clean (zero
  warnings), `rustfmt --edition 2024 --check` clean on all 23
  touched files.

- A5 (2026-11-27): build fn flips. All 15 fns (`assets.rs`:
  `embed_assets`, `try_embed_assets`, `prepare_assets`,
  `try_prepare_assets`, `out_dir_root`, `module_guarded_path`,
  `script_text`, `run_spec`, `build_and_materialize`,
  `clear_materialize_dir`, `ensure_materialize_dir`,
  `asset_input_fingerprint`, `hash_input_path`,
  `stage_materialize`, `sync_tree`) return `DiagnosticResult`;
  18 outer `anyhow::Error::new(ctor(...))` wrappers dropped
  (ctors keep `&anyhow::Error` params as conversion owners,
  byte-identical). One new keyed ctor for the single bare path:
  `operation_failed` (`build.operation.failed`, verbatim reason
  like `proc.run.synthetic_failed`), plus the single owner
  `from_anyhow` (lossless: first keyed `Diagnostic` in the chain
  reclaimed, outer `.context(...)` frames replayed via native
  `with_contexts`, mirroring `lift_dispatch_error`; unkeyed
  fallout wraps verbatim). All direct fs `?`s
  (`PathResolver::from_manifest_env`, `GuardedPath::join`,
  `read_dir_entries`, `read_file`, `entry_kind`,
  `create_dir_all`, `remove_*`, `copy_file`,
  `copy_dir_from_unguarded`, `DirEntry::file_type`) map via
  `from_anyhow`, preserving keyed `fs.*` payloads with frames.
  `gather_assets`/`emit_embed_module`/`write_file` map the same
  way (`syn::Error` via `anyhow::Error::new` then `from_anyhow`,
  verbatim). `script_execution_failed` keeps the A4 bridge
  (`Diagnostic` via `anyhow::Error::from` into the
  `&anyhow::Error` ctor, permanent). Test fns widen to
  `anyhow::Result` (hold flipped callees via `From`,
  `Diagnostic` into `anyhow`). Pin tests extended (distinct key
  + verbatim message + reclaim + frame preservation). anyhow
  parents hold via `From` with no caller changes
  (`oxdock-macros` `compute_fingerprint`/`clear`/`stage`
  `{e:#}` identical: `Display` ignores alternate). anyhow dep
  stays (ctor `&anyhow::Error` params permanent while
  `oxdock-embed` stays anyhow). Gates: `cargo test -p
  oxdock-build` green (27 + 4 + 0), `cargo test -p oxdock-macros`
  green (19 + 1), `cargo check --workspace --all-targets` clean,
  `cargo clippy -p oxdock-build --all-targets` clean,
  `rustfmt --edition 2024 --check` clean on both touched files.
  AMEND follow-up: frame-preservation pin added
  (`from_anyhow(anyhow::Error::new(framed).context("outer
  frame"))` asserts key + `contexts() == ["outer frame"]`,
  mirroring `lift_dispatch_error_preserves_frames_and_exit_codes`).
  Deferred gates executed post-AMEND: full
  `cargo test --workspace` green with zero failures across all
  suites (the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures),
  `cargo clippy --workspace --all-targets` clean (zero warnings),
  `rustfmt --edition 2024 --check` clean on all 3 touched files
  (`assets.rs`, `diagnostics.rs`, `350-plan.md`).

- B6 (2026-11-27): tar driver flip. All 5 fns (`tar.rs`:
  `pack_to`, `unpack_from`, `unpack_auto_from`,
  `unpack_raw_checked`, `drain_entries`) return
  `DiagnosticResult`; 3 outer `anyhow::Error::new(ctor(...))`
  wrappers dropped (`archive_empty`, `archive_truncated`,
  `unsupported_type`). Two new keyed ctors for the bare io
  paths: `pack_failed` (`tar.pack.failed`) and `unpack_failed`
  (`tar.unpack.failed`), both verbatim reason like
  `build.operation.failed`, plus the single owner `from_anyhow`
  (lossless: first keyed `Diagnostic` in the chain reclaimed,
  outer `.context(...)` frames replayed via native
  `with_contexts`, mirroring `lift_dispatch_error`; unkeyed
  fallout wraps verbatim via `unpack_failed`). Tar/flate2/io
  `?`s map via the new owners (`anyhow::Error::new(e)`
  ctor-arg adapters, H2a/H3 pattern); `sanitize_rel` already
  native passes through. The `apply` callback stays
  `anyhow::Result` (external boundary: callers span
  permanent-carrier crates `cli` + net-plugin), lifted at the
  three call sites via `from_anyhow`. Core `unarchive` outer
  bridges via `&anyhow::Error::from(e)` (A4/B2 pattern,
  byte-identical `{reason:#}`); `cli` pack/unpack and
  net-plugin `pack_to` sites hold via `From` with zero changes.
  Gate-caught fix within the bulk: net-plugin `apply_push_tar`
  is a tail-expression return (no `?`), so it bridges
  explicitly with `.map_err(anyhow::Error::from)` (A4
  precedent; downcast/exit pins ride the same node). Pin tests
  extended (distinct keys + verbatim messages + reclaim +
  frame preservation, SEV-2 lesson applied up front). anyhow
  dep stays (callback signature + ctor adapters permanent).
  Gates: `cargo test -p oxdock-utils` green (9 + 0),
  `cargo check --workspace --all-targets` clean, full
  `cargo test --workspace` green with zero failures across all
  suites (the two
  `error: unexpected argument '--invalid-oxdock-flag-xyz'` lines are
  expected fixture stderr, not failures),
  `cargo clippy --workspace --all-targets` clean (zero warnings),
  `rustfmt --edition 2024 --check` clean on all 5 touched files
  (`tar.rs`, `diagnostics.rs`, `handlers.rs`, `session.rs`,
  `350-plan.md`).

- C1 (2026-11-27): per-crate closure audit, no code changes.
  `oxdock-pipe` trivially clean (zero `anyhow` paths in src,
  no dep; only template/README prose mentions). `oxdock-pty`
  closed as permanent carrier: sole production anyhow paths
  are the 4 `&anyhow::Error` ctor params
  (`pty_alloc_failed`, `pty_master_reader_failed`,
  `pty_master_writer_failed`, `pty_spawn_failed`) over
  portable-pty third-party failures, plus test-only carrier
  checks; dep stays, same shape as the `oxdock-build`
  foreign-ctor note. `oxdock-process` stays open: B2 flipped
  the trait, and every remaining anyhow path is
  permanent-shape (`from_anyhow` owners, `&anyhow::Error`
  ctor params/bridges, external `expand.rs`
  `PlaceholderCall` boundary); dep removal waits on B9
  (windows-only `child.rs` `assign_job` + unused
  `job_assign_failed` ctor). Permanent-carrier list extended
  with `oxdock-pty`. Gates: `cargo test -p oxdock-pipe -p
  oxdock-pty` green (zero failures), no code touched so
  workspace state inherits the B6 green run.

- D1 (2026-11-27): final stays recorded, plan-only, no code
  changes. All 11 open items verified against the tree and
  marked `- [!]`: `exit_code_of(&anyhow::Error)` carriers
  (`cli` + core + integration), `check_value_at`/tag.rs
  passthroughs (`toc_option_check_failed` won't-fix),
  func-macro `::anyhow::bail!` codegen, russh `Error`
  assoc types, `macro_input.rs` syn boundary, embed gather
  `Context` chains, `PumpDownstreamClosed` thread design,
  `eval_int_arg_resolve`/`std_pipe_*` adapters over sourced
  `os_pipe_creation_failed`, CLI help/version `bail!`, the
  out-of-scope list, and B9 (accept-as-is, `cfg(windows)`
  gated). Note: `diagnostics-unification.md` is absent from
  this repo, so its :2957 FOLLOW-UP has no target to close;
  the won't-fix lives here as final. D1 closes the last
  open checklist item in Plan 350 (`oxdock-process`, shut
  alongside B9 accept-as-is): every A/B/C/D checkbox now
  reads `- [x]`/`- [!]`, zero open items remain.
  AMEND follow-up: `oxdock-process` shut as permanent
  carrier (dep stays for `from_anyhow`, `&anyhow::Error`
  ctors, external `PlaceholderCall`, `cfg(windows)`
  `child.rs`); permanent-carrier list extended accordingly.
  Gates: no code touched; `cargo test -p oxdock-pipe -p
  oxdock-pty` re-run green (zero failures), workspace state
  inherits the B6 green run.

- Compliance enforcement (2026-11-27): plan-only, no code
  changes. All prior `- [!]` permanent / accept-as-is /
  won't-fix closes REVOKED under the compliance rule (no
  `anyhow`-constructed user-facing errors may stay; `anyhow`
  survives only as transparent transport with a keyed owner on
  at least one side; `&anyhow::Error` ctor params are
  non-compliant; magic strings never acceptable). Reopened as
  `- [ ]` required work: B3 typed-error redesign, B4 fs
  conversion via the keyword-constant leaf move, C `pty` +
  `process` + `build` + `utils` + interim-carrier crates to
  zero construction sites then dep removal, D `exit_code_of`
  migration to `exit_code_of_diag`, `check_value_at`/tag
  normalization, func-macro `bail!` codegen, russh assoc-type
  lift, `macro_input.rs` syn lift, embed gather conversion,
  `PumpDownstreamClosed` keyed sentinel, `std_pipe_*`
  typed-source conversion, CLI help/version keyed owners,
  test-only transport-only + dev-tooling (`xtask`, render)
  exempt-but-no-new-construction, B9 windows conversion with
  Windows CI verification (B3/B4 tracked solely in Section B,
  D heading renamed, no duplicates). Zero `- [!]` remain;
  18 `- [ ]` open (B: 2, C: 5, D: 11). Gates: no code touched;
  workspace state inherits the B6 green run.

- B4a (2026-11-27): fs install cluster (first B4 conversion
  bulk). `install_file_atomic`, `install_file_atomic_host`,
  `stage_bytes_in_dir`, `rename_staged_into_place`
  (`sys/oxdock-fs/.../install.rs`) plus the `WorkspaceFs`
  trait method (`lib.rs`), the mock impl (`mock.rs`), and the
  render `EmbeddedFiles` delegation (`embedded.rs`) return
  `DiagnosticResult`; `oxdock-diagnostics` added to
  `oxdock-render` deps. New single owner `from_anyhow`
  (lossless: first keyed `Diagnostic` reclaimed, outer frames
  replayed via native `with_contexts`; unkeyed fallout wraps
  verbatim via new `install_failed(&str)`, no `&anyhow::Error`
  param) plus 4 typed-source install ctors
  (`fs.install.stage_failed/set_mode_failed/place_failed/no_parent`,
  `std::io::Error` sources like `cache_ensure_failed`, no
  `&anyhow::Error` params). Anyhow interiors lift via
  `from_anyhow` (`ensure_parent_dir`, `check_access`,
  `recheck_resolved_source`, backend read/write); bare io
  sites map via the new typed ctors. Core `install` holds via
  the B2 bridge shape (`.map_err(anyhow::Error::from)` into the
  `&anyhow::Error` eval ctor). Pin tests extended (distinct
  keys + verbatim messages + reclaim + frame preservation).
  Gates: `cargo test -p oxdock-fs` green (72 + 8 + 4 + 1),
  `cargo test -p oxdock-core --lib` green (433), full
  `cargo test --workspace` green (93 targets, 2715 passed, 0
  failed; the two `error: unexpected argument
  '--invalid-oxdock-flag-xyz'` lines are expected fixture
  stderr, not failures), `cargo clippy --workspace
  --all-targets` clean (zero warnings), `rustfmt --edition
  2024 --check` clean on all 6 touched files.

- B4b (2026-11-27): fs io cluster (second B4 conversion
  bulk). All `PathResolver` io helpers (`io.rs`: `create_dir_all`,
  `read_dir_entries`, `read_file`, `open_read/write/append`,
  `open_read/write_nofollow`, `refuse_link_ancestors`,
  `read_to_string` x2, `write_file`, `append_file`,
  `ensure_parent_dir`, `canonicalize`, `metadata`,
  `archive_member_stat`, `entry_kind`, `entry_kind_no_follow`,
  `metadata_unguarded`, `set_permissions_mode_unix` x2,
  `open_file_unguarded`, `remove_file`, `remove_dir_all`,
  `remove_empty_dir`, `remove_link_if_symlink` x2, spill
  `create/open/truncate` x2, `write_private_file` x2,
  `SpillFile::set_len`, `symlink` x2) return
  `DiagnosticResult`. Transitional lift throughout, no new
  typed ctors: still-anyhow interiors (`check_access`,
  `check_access_with_root`, `recheck_resolved_source`,
  `resolve_write`, backend) lift via the single owner
  `from_anyhow` with native `with_contexts` frames preserving
  every historic context string byte-identical;
  already-keyed `anyhow::Error::new(fs_diag::...)` sites
  return the diagnostic directly; bare `std::io` sites lift
  via `anyhow::Error::from` transport into `from_anyhow` plus
  frames (no `.with_context`, no `Error::new`, no `Context`
  import). `from_anyhow` fallback generalized from
  install-specific to the new generic
  `operation_failed(&str)` (`fs.operation.failed`, mirroring
  `build.operation.failed`); `install_failed` kept for
  install-specific fallout. `WorkspaceFs` trait surface
  stays anyhow: `PathResolver` impls bridge via `Ok(...?)`
  (A1 pattern), as does render `EmbeddedFiles`
  (`install_file_atomic` already native passes through).
  Cascade callers updated minimally: core `handlers.rs`
  direct `refuse_link_ancestors` sites bridge via the
  `&anyhow::Error::new(e)` ctor-arg adapter (H2b precedent);
  build `assets.rs` flipped-method sites drop the now-useless
  `.map_err(from_anyhow)` to native `?` (same keyed payload)
  and adapter-bridge the three `&anyhow::Error` ctors;
  mcp/cli/ssh-plugin ctor sites take the same adapter;
  `is_io_kind` guard clones into `anyhow::Error::from`;
  anyhow test helpers take `Ok(...?)`. Pin tests extended
  (new `operation_failed` key + message, fallback key
  updated to `fs.operation.failed`). Gates: `cargo test -p
  oxdock-fs` green (72 + 8 + 4 + 1), `cargo test -p
  oxdock-core --lib` green (433), full `cargo test
  --workspace` green (93 targets, 2715 passed, 0 failed),
  `cargo clippy --workspace --all-targets` clean (zero
  warnings), `rustfmt --edition 2024 --check` clean on all
  touched files.

- B4c (2026-11-27): fs backend cluster (third B4 conversion
  bulk). `BackendImpl` trait plus `HostBackend`, `MiriBackend`,
  the `Backend` delegator, `fd_path` (linux/macos/windows),
  `verify_fd_within` x2, `synthetic_metadata`, and every
  `SyntheticRootState` helper (`backend.rs`) return
  `DiagnosticResult`. Transitional lift throughout, no new
  typed ctors: bare `std::io` sites lift through two private
  helpers (`lift_io_err`/`lift_io_ctx`: `anyhow::Error::from`
  transport into the single owner `from_anyhow`, historic
  context string replayed natively via `with_contexts`), so the
  eventual typed-ctor migration touches one place, not 20+
  call sites. Every historic context string stays
  byte-identical with one deliberate exception: Linux `fd_path`
  now emits keyed `fs_diag::fd_path_failed(&e)` (matching the
  macOS/Windows arms) instead of the old untyped
  `"failed to resolve open file path"` context frame.
  Already-keyed `anyhow::Error::new(fs_diag::...)` sites
  (`fd_path_failed`, `opened_outside_tree`,
  `unsupported_file_type`, `workdir_not_dir`,
  `copy_source_missing`, `copy_source_unsupported_kind`,
  all `synthetic_*`) return the diagnostic directly (no
  `Error::new`, no `.with_context`, no `Context` import;
  `MiriBackend::remove_empty_dir` attaches its frame straight
  onto the already-keyed `Diagnostic`, never round-tripping
  through `anyhow`).
  `install.rs`/`io.rs` drop all obsolete
  `.map_err(fs_diag::from_anyhow)` wrappers on `self.backend.*`
  calls (now native `DiagnosticResult` passthrough);
  still-anyhow `resolve.rs` bridges via the A1 pattern
  (`Ok(self.backend...?)`, keyed owner on the backend side).
  Gates: `cargo test -p oxdock-fs` green (72 + 8 + 4 + 1),
  `cargo test -p oxdock-core --lib` green (433), full `cargo
  test --workspace` green (93 targets, 2715 passed, 0 failed),
  `cargo clippy --workspace --all-targets` clean (zero warnings),
  `rustfmt --edition 2024 --check` clean on all 4 touched
  files (`backend.rs`, `io.rs`, `install.rs`, `resolve.rs`).

- B4d (2026-11-27): fs copy cluster (fourth B4 conversion
  bulk). `entry_kind_follow_symlink`, `recheck_resolved_source`,
  `copy_file` x2, `copy_dir_recursive` x2,
  `copy_dir_from_unguarded`, `copy_file_from_unguarded`
  (`copy.rs`) return `DiagnosticResult`. Transitional lift
  throughout, no new typed ctors: still-anyhow interiors
  (`check_access`, `check_access_with_root`,
  `GuardedPath::new`) lift via the single owner `from_anyhow`
  with native `with_contexts` frames preserving every historic
  context string byte-identical; already-keyed
  `anyhow::Error::new(fs_diag::unsupported_file_type)` returns
  the diagnostic directly; bare `std::io` sites
  (`fs::metadata`, `fs::read_dir`, entry/`file_type`,
  `fs::create_dir_all`, `fs::copy`) lift through two private
  helpers (`lift_io_err`/`lift_io_ctx`: `anyhow::Error::from`
  transport into `from_anyhow`, historic string replayed
  natively), mirroring `backend.rs`, so the eventual typed-ctor
  migration touches one place per file. Already-native callees
  (`recheck_resolved_source`, `create_dir_all`,
  `entry_kind_follow_symlink`, `copy_*` recursion, backend
  read/write/listing) attach frames straight onto the returned
  `Diagnostic`, never round-tripping through `anyhow` (B4c
  lesson). `WorkspaceFs` trait surface stays anyhow: `lib.rs`
  impls bridge via `Ok(...?)` (A1 pattern); render
  `EmbeddedFiles` delegation bridges the same way (4 sites).
  Cascade callers updated minimally: `io.rs` `symlink` x2 and
  `install.rs` drop the obsolete `.map_err(from_anyhow)` on
  `recheck_resolved_source` (now native passthrough); build
  `assets.rs` `sync_tree` drops the now-useless
  `.map_err(from_anyhow)` on `copy_file` to native `?`, and
  `stage_materialize` bridges the `&anyhow::Error`
  `stage_copy_failed` ctor via `&anyhow::Error::from(e)` (A4/B2
  shape, byte-identical `{reason:#}`).
  Gates: `cargo test -p oxdock-fs` green (72 + 8 + 4 + 1),
  `cargo test -p oxdock-core --lib` green (433), `cargo test
  -p oxdock-build` green (27 + 4 + 0), full `cargo test
  --workspace` green (93 targets, 2715 passed, 0 failed),
  `cargo clippy --workspace --all-targets` clean (zero warnings),
  `rustfmt --edition 2024 --check` clean on all 6 touched
  files (`copy.rs`, `lib.rs`, `io.rs`, `install.rs`,
  `assets.rs`, `embedded.rs`).

- B4e (2026-11-28): fs guard + cache cluster (fifth B4
  conversion bulk). `guard_path`, `normalize_candidate`,
  `guard_path_miri`, `check_access_with_root`, `check_access`
  (`access.rs`) and `ensure_cache_dir` (`cache.rs`) return
  `DiagnosticResult`. Transitional lift throughout, one typed
  ctor: bare `std::io` sites (`create_dir_all` root,
  `canonicalize` root/ancestor) lift through two new private
  helpers (`lift_io_err`/`lift_io_ctx`: `anyhow::Error::from`
  transport into the single owner `from_anyhow`, historic
  string replayed natively via `with_contexts`), mirroring
  `backend.rs`/`copy.rs`; already-keyed
  `guard_access_escapes`/`guard_snapshot_not_materialized`
  sites return the diagnostic directly; `ensure_cache_dir`
  uses the typed-source `cache_ensure_failed(dir, &e)` ctor, no
  lift. Every historic context string stays byte-identical.
  Cascade callers updated minimally: `io.rs` (28 sites),
  `copy.rs` (10), `install.rs` drop the now-obsolete
  `.map_err(from_anyhow)` on native guard callees (native `?`
  passthrough); bare-io lifts
  (`.map_err(anyhow::Error::from).map_err(from_anyhow)`) and
  still-anyhow bridges (`GuardedPath::new`,
  `resolve_write` match arms in `remove_link_if_symlink`)
  kept. Still-anyhow `resolve.rs` `resolve` bridges via
  `Ok(...?)` (A1 pattern). One script over-removal caught by
  `cargo check` and repaired: the `remove_link_if_symlink`
  `None` arm and `copy_dir_from_unguarded`
  `GuardedPath::new` bridge restored.
  Gates: `cargo test -p oxdock-fs` green (72 + 8 + 4 + 1),
  full `cargo test --workspace` green (93 targets, 2715
  passed, 0 failed), `cargo clippy --workspace --all-targets`
  clean (zero warnings), `rustfmt --edition 2024 --check`
  clean on all 6 touched files (`access.rs`, `cache.rs`,
  `io.rs`, `copy.rs`, `install.rs`, `resolve.rs`).

- B4f (2026-11-28): fs resolve cluster (sixth B4 conversion
  bulk). All 11 `PathResolver` resolution methods
  (`resolve_workdir`, `resolve_workdir_system`,
  `resolve_system`, `resolve_read`, `resolve_write`,
  `resolve_copy_source`,
  `resolve_copy_source_from_workspace`,
  `resolve_copy_source_from_target`,
  `resolve_copy_source_from_root`, `resolve`,
  `parse_env_path`) return `DiagnosticResult`. Transitional
  lift for the two still-anyhow callees (`ensure_cache`,
  `rebased_cwd`) via the single owner `from_anyhow`;
  already-keyed `copy_no_workspace_root` /
  `copy_snapshot_not_materialized` return the diagnostic
  directly via `ok_or_else` ctor; all 7 historic
  `.with_context` strings (`WORKDIR {} escapes root` x2,
  `failed to resolve COPY source {}` x5) replayed
  byte-identical via native `with_contexts`; already-native
  guard/backend callees pass through directly (no `Ok(...?)`
  wrapping, per clippy `needless_question_mark`). Zero
  `anyhow::Error::new`, `.with_context`, `use anyhow` remain
  in `resolve.rs`. Cascade callers updated minimally: `io.rs`
  drops 2 obsolete `.map_err(from_anyhow)` on
  `resolve_write` (native passthrough); `lib.rs` `WorkspaceFs`
  impls bridge via `Ok(...?)` (6 sites, A1 pattern); render
  `embedded.rs` delegation bridges the same way (6 sites);
  still-anyhow `git/mod.rs` `copy_from_git` bridges 9 native
  call sites via `.map_err(anyhow::Error::from)` ahead of the
  existing `with_context` frames. One regex over-strip caught
  by `cargo check` and repaired: `resolve.rs` reverted and
  redone with surgical replacements. Gates: `cargo test -p
  oxdock-fs` green (72 + 8 + 4 + 1), full `cargo test
  --workspace` green (93 targets, 2715 passed, 0 failed),
  `cargo clippy --workspace --all-targets` clean (zero
  warnings), `rustfmt --edition 2024 --check` clean on all 5
  touched files (`resolve.rs`, `lib.rs`, `io.rs`,
  `git/mod.rs`, `embedded.rs`).

- FRAME-DEBT (2026-11-28, recorded not fixed per user
  decision): the B4d-B4f transitional `with_contexts`
  frames hardcode wording at call sites (~38 `format!`
  sites across `io.rs`, `copy.rs`, `install.rs`,
  `access.rs`, `resolve.rs`), violating the diagnostics
  module contract (wording owned by `diagnostics.rs`, call
  sites pass structured data only, never `format!`) and
  duplicating wordings (`creating dir` x8, `denied for`
  variants, `WORKDIR escapes root` x2, `COPY source` x5).
  Direct replacement for each site is a typed frame ctor in
  `diagnostics.rs` taking only structured data (e.g. the
  path), with the call site passing the value through.
  Remaining B4 flips (`path.rs`, `git/`, `mod.rs`,
  `mock.rs`, `policy.rs`, `lib.rs` top-level) continue with
  the transitional pattern; the centralization pass stays
  scheduled debt, not a silent accept.
