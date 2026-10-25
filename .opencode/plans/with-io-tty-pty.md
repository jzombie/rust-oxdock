# Plan: shared pty core + `WITH_IO [tty]` (and actions cleanup)

## Goal

Give `RUN` steps an opt-in pty without new top-level keywords, on a
shared pump core both the SSH plugin and local runs consume. Then
migrate the install-smoke `isolated` leg off its per-OS `script(1)`
hack onto the new surface, and remove the dead shell from the action
workflows. No change to default `RUN` stdio: hermetic pipes stay the
default everywhere.

## Background and hard constraints

- Decision record (`AGENTS.md`, Process-Manager Boundary):
  `oxdock-process` stays sync/std/Miri-clean; thread+channel event
  loops live outside it. The pty pump therefore lives in a NEW crate,
  never in `oxdock-core` or `oxdock-process`. The sync core only sees
  a blocking `spawn → pump → join` call that is synchronous from the
  DSL's point of view.
- `crates/plugins/oxdock-ssh-plugin/src/pty.rs` (~800 lines) already
  implements most of the core: `portable-pty` allocation, pipe↔master
  pump workers, ConPTY DSR query filter + `1;1` reply, drain
  quiescence policy (`drain_overdue`, pure and unit-tested), exit
  reap, scoped teardown. What stays plugin-local: session `PtySize`
  state, the SSH `bridge` (sinks, close-drain), the `on_exit`
  reporter ordering, and the `ESC[2J ESC[H` console-clear init (remote
  parity behavior, not wanted for local runs).
- `WITH_IO` already owns stdio routing, so `tty` becomes a binding
  flag there, not a new keyword. Future knobs (`rows`, `cols`, maybe
  `term`) become more `WITH_IO` keys, never new commands.
- Miri: pty allocation cannot run under Miri isolation. New tests
  carry `#[cfg_attr(miri, ignore = "<reason>")]`; the crate itself
  must still compile under Miri (no `std::process` at module scope
  that breaks the build).
- Every new crate ships a pipeline README (`cargo run -p docs-gen`
  renders it from a `<crate>/.oxdock/template/` tree); commit the
  output. No em dashes anywhere; localized `#[allow]` only, and only
  the `crates/sys/*` latitude covers the new crate's OS calls.

## Workstream 1: `oxdock-pty` shared crate

- New crate `crates/sys/oxdock-pty` (workspace membership is by glob;
  confirm it picks up the new dir, else register explicitly).
- Move from the ssh plugin, unchanged in behavior: pair allocation
  with initial size, master reader/writer split, input worker
  (pipe/bytes → master), output worker (master → pipe/sink with DSR
  filter), resize application (`TIOCSWINSZ` / ConPTY resize), drain
  quiescence wait + cap, exit-code reap, scoped join with kill on
  cancel/drop. Keep the pure `drain_overdue` policy with its unit
  tests (move the tests with the code).
- Leave behind: `PtySize`/`SharedPtySize` session state, `bridge`
  sinks, `on_exit` callback shape (becomes a generic
  `on_exit: Option<Arc<dyn Fn(u32)>>` if the core keeps the hook, or
  drops it and returns the code), the clear-screen init write.
- Rewire `SSH_PTY_RUN` onto the crate with zero behavior change.
  Proof: existing ssh pty tests + the smoke ssh legs stay green
  without modification.

## Workstream 2: `WITH_IO [tty]` grammar and lowering

- Grammar: bare `tty` flag plus `rows = <int>` / `cols = <int>`
  keys inside `WITH_IO [...]`. Reserve the `tty` name: `tty = $x`
  and unknown tty-adjacent keys fail at parse with the valid shapes.
- Validity: `tty` outside a `RUN` step is a lowering error naming
  the restriction. A pty master is one interleaved stream: `stderr`
  bindings alongside `tty` are a lowering error stating exactly that
  (stdout and stderr cannot demultiplex). `stdin = $p` feeds the
  master; bare `[tty]` leaves stdin closed to the child.
- Static checks ride the existing `WITH_IO` validation path; no new
  step kind, no new top-level keyword.

## Workstream 3: core wiring (still no event loop in core)

- `RUN` under `WITH_IO [tty]` spawns through `oxdock-pty` with the
  step-scoped pump: spawn → pump → join, blocking the step like any
  `RUN`. Cancellation kills the child (same path as `CANCEL`).
- Master output routes exclusively to `stdout` (step capture or
  the `stdout = $p` pipe): it carries the child's interleaved stdout
  and stderr, so pty assertions target `stdout` only. `ASSERT_*`
  otherwise works unchanged (authors must expect pty translation:
  `\n` arrives as `\r\n`, prompts echo input).
- Pipe promotion must not touch pty stdio: pty-backed descriptors
  are never promotable OS pipes; assert this in the promotion walk
  with a dedicated test.
- Env/anchors follow `RUN` exactly (closed child env, `INHERIT_ENV`
  routing, spawn-anchor checks); working directory follows the step.

## Workstream 4: actions cleanup

- `install-smoke.yml` `isolated` leg: replace the `[os:linux]` /
  `[os:macos]` `script -qec` / `script -q` branches and the `sh -c`
  quoting with one portable stanza, e.g.
  `WITH_IO [tty, stdin=$answer] RUN ["bash", "./install.sh"]`
  (and the `pwsh -File` twin for Windows, which the shared crate
  covers via ConPTY, closing the current Unix-only gap).
- Delete the now-dead `script(1)` readiness around the prompt and
  the per-OS prompt branches. Keep coverage identical: prompt
  install lands the prompted dir, version asserts, `GITHUB_PATH`
  silence asserts stay.
- Conformance: bump `smoke_script_conformance` markers to require
  the new stanza shape (`WITH_IO [tty` presence) so a revert to
  shell hacks fails loudly.

## Workstream 5: tests, docs, conformance

- Crate unit tests: drain policy (moved), DSR filter edge cases
  (split queries, flush-at-EOF), resize application.
- Integration (run on runners, `ignore`d under Miri with reasons):
  prompt loop end-to-end (the isolated-leg scenario as a hermetic
  tempdir test), `\r\n` translation assertions, no-tty-by-default
  regression (bare `RUN` still sees pipes/null).
- `#[example]` blocks on the new surface (they execute under
  `docs_conformance`); reference docs regenerate via docs-gen.
- No new `ENV` names, so no `ENV_CONTRACTS` changes.

## Sequencing (landable slices)

1. Crate extraction + ssh rewire. No behavior change; ssh tests
   and smoke ssh legs are the proof. Land alone.
2. Grammar + lowering + core wiring behind the new bindings.
3. Leg migration + dead-shell deletion + conformance bumps.
4. Docs-gen render + README commit for the new crate.

## Out of scope (separate tickets)

- Freeze/suspend primitives (`TASK_FREEZE`/`TASK_THAW`).
- DAP adapter or any #128 debugging surface.
- Changing default `RUN` stdio (stays hermetic pipes/null).
- `WITH_TTY` as a standalone command (rejected in favor of the
  `WITH_IO [tty]` flag; do not add it later without revisiting).
- Windows `script(1)`-style coverage anywhere else; the pty
  capability is covered, per-leg shell workarounds are not expanded.

## Verification

- `cargo test --workspace --tests`, `cargo clippy`, `cargo fmt`,
  `cargo run -p docs-gen` clean with committed outputs.
- New crate's tests green natively, properly `ignore`d under Miri.
- Full `install-smoke.yml` matrix green, including the rewritten
  `isolated` leg on all five runners and the prompt path without
  `script(1)`.
