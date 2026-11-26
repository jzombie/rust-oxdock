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
      Decision:
- [ ] B2 `ProcessManager` trait (`contract.rs`,
      `shell_manager.rs`, `mock.rs`, `synthetic.rs`,
      `child.rs`). Flip trait to `DiagnosticResult` OR declare
      anyhow-at-the-sys-boundary permanent.
      Decision:
- [ ] B3 `RemoteRunner` trait + `TransferStage`
      (`net session.rs` 36 sites, `core/remote.rs`
      `run_remote_block`). Redesign OR close as permanent
      (concrete-error params risk flattening chained
      `RpcServiceError`/`FrameEncodeError` io, proven in the
      fetch bulk).
      Decision:
- [ ] B4 `oxdock-fs` exclusion (~200 sites: `io.rs` 68,
      `backend.rs` 37, `copy.rs` 27, `git/` 27, `install.rs`
      12, `path.rs` 10, ...). (a) Permanent exclusion: fs
      keeps literals, stays anyhow; OR (b) move shared keyword
      constants to a leaf crate to break the parser cycle,
      then standard bulk conversion. Largest scope decision
      left.
      Decision:
- [ ] B5 `ForwardError` redesign (small, self-contained, no
      input needed): `chase_const_forward` ->
      `DiagnosticResult`; unblocks `import_const_forward_failed`
      + `check_ctx_wrap` sites (`import.rs`, `static_check.rs`
      :1022,:5166).
- [ ] B6 `oxdock-utils/tar.rs` pack/unpack (3 sites): needs io
      ctors + `apply`-callback migration design first.
- [ ] B7 Closure cascade: `write_stdout`/`write_stderr`
      (`core/io.rs:368,392`) + 10+ inline closures in
      `handlers.rs`/`remote.rs`. Flip as one cascade with
      callers, inside B1's blast radius.

## C. Per-crate closure (grep `anyhow::` paths -> remove dep -> gates)

- [x] `oxdock-diagnostics` (anyhow dev-only already)
- [ ] `oxdock-pty`
- [ ] `oxdock-process` (after B2; windows-only `child.rs`
      needs B9)
- [ ] `oxdock-pipe`
- [ ] Keep permanently as carrier/macro-boundary (staged
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

## E. Suggested order

1. Decide B1 -> unlocks A1-A4, A6, A2.
2. Decide B4 -> settles ~200 sites (fs chapter: convert or
   close as permanent).
3. Decide B2/B3 -> end-state of process + net session.
4. Run bulks (H1-H6b file-sized), B5/B6 designs, B7 inside B1.
5. Section C per-crate closure.
6. Section D recorded final; future audit reads zero open items.
