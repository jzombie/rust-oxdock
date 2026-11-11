# Plan: dry-run validation parity + multiline FUNC params

Source: user report that `oxdock --dry-run tools.oxdock` passed while `oxdock --mcp tools.oxdock` failed on static `RETURN TEXT_TRIM` union mismatch, plus request for multiline `FUNC` param lists including generics.

## 1. Objectives

- `--dry-run` must fail whenever execution or `--mcp` would fail on static validation. No false pass.
- `FUNC` param lists split across lines must parse, including generic `MAP<...>` / `LIST<...>` shapes that already allow newlines via `skip_type_nl`. No duplicated newline logic.
- No piecemeal triplication: one shared validation helper, one shared gap helper.

## 2. Shared validation helper (oxdock-core)

- Add `pub fn validate_steps_for_run(state, steps)` in `crates/oxdock-core/src/exec/mod.rs` doing exactly what `finish_run` and `McpSession::init` both duplicate today:
  - `collect_type_aliases`, `validate_function_type_tags`, `expand_import_closure`, `validate_script_closure`, with alias save and restore.
- Rewrite `finish_run` and `McpSession::init` to call it. No behavior change, only dedup.
- Add `pub fn validate_cli_steps(build_context, steps, modules, types, entry_file)` that builds a throwaway `ExecState` via `new_state` plus `build_module_table` and `register_module` / `register_type`, then calls the shared validator. Dry-run uses this, so it validates with the same modules and table as execution.
- `print_dry_run` in `crates/oxdock-cli/src/lib.rs` calls the validator before `dry_run_closure`. Both `execute_with_result` and `execute_with_shell_runner` dry-run branches gain the same call. Validation errors abort before any listing prints.

## 3. Multiline FUNC params (oxdock-parser)

- Root cause: `parse_func_at` in `crates/oxdock-parser/src/parser.rs` uses `Cursor::expr` plus `skip_gap` inside the paren group. Newlines miss to generic, which then fails as `unexpected line break in argument`.
- Fix in place with existing helpers only:
  - Switch the param loop junctures from `skip_gap` to `skip_gap_nl` (already used for call args, lists, maps). `skip_gap_nl` is the shared helper, not a copy.
  - `parse_type_shape` already tolerates newlines via `skip_type_nl`, so generics ride free with no extra code.
  - Ensure trailing newline before `)` is skipped before the `done` check.
- Outer header newlines between `)` and `:` and `{` stay single line for now. Inner splits cover the reported long param case. If outer splits are later needed, that requires `split_statements` awareness, separate follow-up.

## 4. Tests

- CLI dry-run parity test: fixture script with `FUNC F(): STRING { RETURN TEXT_TRIM("x") }` must fail under `--dry-run` with the same union message `--mcp` reports.
- Parser tests: multiline `FUNC FOO(\n $x: STRING,\n $y: MAP<a: STRING, b: INT>\n): STRING` parses with correct params and return. Single line cases unchanged.
- Workspace `cargo test --workspace --tests` and `cargo clippy --workspace --tests` clean. `cargo run -p oxdock-render` shows no drift.

## 5. Verification

- `oxdock --dry-run tools.oxdock` fails before fix, passes after the tools file gate fixes already applied.
- `oxdock --mcp tools.oxdock` starts to initialize prompt (no parse error).
- Multiline repro from this thread parses.
