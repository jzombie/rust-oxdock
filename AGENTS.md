# OxDock Workspace Guardrails

## Linting

Do not override linter rules in any crate. The only permitted narrow, crate-local `#[allow(...)]` exceptions are the system crates located under `crates/sys/*` (for example, `oxdock-fs`, `oxdock-process`, `oxdock-sys-test-utils`). Never add workspace-wide or blanket overrides.

- **Naming**: Do not prefix parameter names with `_` to silence unused warnings. Prefer using the value (e.g., `let _ = param;`) or a localized `#[allow(unused_variables)]` if absolutely necessary.

- **Strategy**: When addressing clippy warnings, prefer localized `#[allow(...)]` on specific items (functions/impls/structs) instead of crate-level `#![allow(...)]`.
    - Avoid blanket overrides on traits or impl blocks if the lint only applies to a subset of methods; apply the suppression to the individual methods instead.
    - For imports, split the `use` statement so that `#[allow(...)]` only applies to the specific disallowed item.
    - For enums/structs, apply `#[allow(...)]` to the specific variant or field that uses the disallowed type, not the whole type definition.
- **Scope**: Do not perform repository-wide or file-wide blanket changes to satisfy lints; limit edits to the minimal, justified scope.
- **Enforcement**:
    - `clippy.toml` is used to `deny` `disallowed_macros` (like `std::cfg`), `disallowed_methods`, and `disallowed_types`.
    - `[workspace.lints.clippy]` in `Cargo.toml` enforces these rules workspace-wide.
    - Crates should not override these defaults unless absolutely necessary (rare).

## Miri & Isolation

Prefer explicit, test-only skips over runtime detection.

- **Principle**: Avoid runtime Miri detection in implementation code. Tests *can* be skipped under Miri only when necessary, and they *MUST* include the reason.
- **Implementation**:
    - Do not use `cfg!(miri)` or `#[cfg(miri)]` outside of `crates/oxdock-fs` and `crates/oxdock-process`, except for test-only `#[cfg_attr(miri, ignore = "...")]` skips that include a reason.
- **Exceptions**:
    - Implementation crates (`oxdock-fs`, `oxdock-process`) may legitimately use `cfg!(...)` or `#[cfg(...)]` internally.
    - Use narrow, localized `#[allow(clippy::disallowed_macros)]` or `#[allow(clippy::disallowed_methods, clippy::disallowed_types)]` on specific functions/items to keep the rest of the crate provably conformant.

## Filesystem & Process

- **Filesystem**: Use the `oxdock-fs` abstractions (`GuardedPath`/`UnguardedPath`, `PathResolver`, `GuardedPath::tempdir`) instead of raw `std::fs` for guarded paths; keep paths under their guards.
    - Do not use `tempfile` directly in any crate other than `oxdock-fs`.
    - Normalize paths via the shared helpers (`to_forward_slashes`, `normalized_path`, `PathResolver::parse_env_path`) instead of ad-hoc `.replace` chains or string comparisons.
- **Process Execution**: Use `oxdock-process` abstractions instead of raw `std::process::Command`.
    - Logic handling process execution differences (e.g. Miri vs Native) belongs in `oxdock-process`.

## Cross-Platform Compatibility

- **Consistency**: All features and DSL commands must behave identically across platforms (Linux, macOS, Windows) as much as possible.
- **Exceptions**: Only `RUN` and `RUN_BG` commands are expected to differ, as they execute arbitrary shell commands specific to the host OS.
- **Testing**: Tests must ensure parity. If platform-specific setup is required (e.g. creating symlinks in test fixtures), ensure both Unix and Windows paths are covered.

## Testing & Layout

- **Testing**: Prefer `cargo test --workspace --tests` to cover all crates; fixtures for the macros live under `crates/oxdock-logic-tests/fixtures/integration/buildtime_macros`.
- **Workspace layout**: Internal crates live under `crates`; the CLI & build-time macros sit at the workspace root.

## Process-Manager Boundary (decision record)

`oxdock-process` and term-wm's PTY stack stay **independent**: batch script-to-completion orchestration vs. interactive PTY/vt100 streaming are disjoint mechanics with conflicting constraints (sync/std/Miri-clean vs. thread+channel event loops). The only genuine overlap — `shell_program()` shell resolution and `is_pid_alive()` liveness probing in `oxdock-fs` (backing the tempdir PID-lock GC) — is behavior-pinned by tests, so if duplication ever hurts, extraction into a small shared micro-crate is a mechanical follow-up. Do not introduce cross-dependencies between these projects without revisiting that assessment.

## Workflow

- **Autonomy**: When test failures are reported or observed, proceed to investigate and fix them without asking for confirmation unless there are multiple viable options or the change is risky/behavior-altering.
- **Formatting**: Use `indoc` for multi-line Rust string literals in tests or fixtures when formatting clarity matters.
