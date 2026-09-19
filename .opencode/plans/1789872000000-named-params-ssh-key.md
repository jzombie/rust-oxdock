# Plan: Named/Optional Function Parameters + Stable SSH Host Key

## Goal (in user words)
1. Functions take **named parameters, Python-style, forced** (no positional ambiguity), with **optional params backed by Rust `Option`**, declared via a **struct for the Rust args**.
2. On top of that: `SSH_SERVE` gains a stable host key (load-or-create key file) so restarts don't invalidate `known_hosts`.

## Part A: Named parameters (the bullet to bite)

### A0. Scope decision (recommended)
- **New capability, opt-in per function; forced within it.** A function is either positional (today's model, unchanged) or named (every arg must be `name = value`; any positional arg is an error, and vice versa). No mixed calls, no silent fallback.
- **Migrate the whole SSH module to named at once** (5 funcs; alpha, our crate, purpose-built to dogfood this). STD builtins and user `FUNC` defs stay positional in v1 — no churn to existing scripts/tests, no breaking change to the language core.
- **Named funcs are AST-only** (never RPN): RPN is a positional stack and cannot express names. `pure` + named is rejected by the macro (pure keeps implying `rpn`).

### A1. Syntax: `NAME(k = v, ...)` (recommended)
- `=` (matches Python and `WITH_IO` bindings).
- Grammar (`crates/oxdock-parser/src/dsl.pest`): extend `func_call` and `call_statement` arg lists with a `named_arg = { ident ~ "=" ~ !"=" ~ expr }` alternative. The `!"="` negative lookahead is load-bearing: without it PEG matches the first `=` of `==` and corrupts positional calls like `FOO(a == b)` (consumes `a =`, leaves `= b` unparseable). Ordered choice tries `named_arg` first, falls back to positional `expr`. No trailing commas (unchanged), no `=` in RPN math positions.
- Ordering: any order (that's the point). Duplicates: parse error. Param naming convention: `UPPER_SNAKE` (DSL convention, consistent with func names).

### A2. AST + lowering
- `crates/oxdock-parser/src/ast.rs`: `Expr::Call` and `StepKind::Call` gain `args: Vec<CallArg>` with `CallArg { name: Option<String>, expr: Expr }` (`None` = positional; existing constructors default to `None`).
- `Display` renders `NAME(k = v)` / `NAME(v)` faithfully (round-trip).
- `MathOp::Call{name, arity}` unchanged. Named calls are rejected at lowering time (see A3): they never reach RPN.
- `oxdock-macros` (`emit_stepkind`, `emit_expr`): emit the new shape.

### A3. Runtime binding (by name, step-numbered errors)
- `handlers::call_func_value` + `args::evaluate_expr` (Call arm): after resolving the callee, branch on its model:
  - Positional callee + any named arg → `step N: F() takes positional arguments, got named 'k'` (and symmetrically for named callee + positional arg).
  - Named callee: unknown name → `step N: F() has no parameter 'k' (expected: A, B, ...)`. Missing required → `step N: F() missing required argument 'k'`. Exact-duplicate names already rejected at parse.
  - Key normalization (load-bearing): both sides canonicalize with `to_ascii_uppercase()` before map insert/lookup, so `bind = ...`, `BIND = ...`, and `Bind = ...` all bind `BIND`. The evaluator normalizes at insert; a normalized key already present → `step N: F() duplicate argument 'K'` (catches case-variant dupes like `bind=1, BIND=2` that parse can't see). Unknown-name and missing-required messages use the normalized form. Macro field `name = ...` overrides are normalized identically.
  - Evaluate values left-to-right (source order), then hand off as a `Value::map` (string-keyed `MAP`, `BTreeMap<String, Value>` — already a first-class word type). Rationale: `NativeFn` takes a flat `Vec<Value>` and `Value` has no `Null`/`Unset`, so positional assembly cannot represent a sparsely omitted `Option` (e.g. `FOO(req="x", opt2=42)` with `opt1` omitted would shift `opt2` into `opt1`'s slot and fail extraction). The map handoff keeps every name attached; no new `Value` word type, positional path untouched.
- New `NamedNativeFn<P> = Arc<dyn Fn(&mut StepCtx<P>, BTreeMap<String, Value>) -> Result<Value> + Send + Sync>` plus a `HostRegistration::StatefulNamed` (or equivalent flag + entry variant) carrying it; `ExecState::register_module` dispatches on the variant. Macro-generated code extracts per key: absent + `Option<T>` → `None`, absent + required → `step N: ... missing required ...` (defense in depth; runtime already checked), present → existing scalar extractor messages.
- Validation stays **runtime** (like arity today), not parse-time: `ModuleTable` only carries names, and extending it is optional hardening (noted, not planned).
- RPN: named calls are intercepted at lowering time in `crates/oxdock-parser` (the `Expr::Call` → `MathOp::Call` site, `parser.rs` near `ops.push(MathOp::Call`): any `CallArg` with `name.is_some()` is a **parse-time** error `named arguments are not supported in RPN math expressions`. Never a runtime `unknown function` (that misattributes a structural grammar constraint as a registration problem).

### A4. Rust declaration model: struct + `Option` (as the user intuited)
- `#[oxdock_func]` on a function whose last param is an args struct, e.g.:
  `struct SshServeArgs { bind: String, username: String, password: String, key_path: Option<String> }`
  `fn ssh_serve(cx: &mut StepCtx<P>, args: SshServeArgs) -> Result<Value>`
- Macro (`crates/oxdock-func-macro/src/lib.rs`): recognize a final non-`Value/String/i64/f64/bool` type as the args struct; reflect its named fields (closed set: same scalar types, each optionally wrapped in exactly one `Option`); generate map-keyed extraction (absent → `None` for `Option`, step-numbered missing-required bail otherwise) using the existing extractor messages. Field name uppercased by default (`bind` → `BIND`), overridable per field. The generated entry is the `NamedNativeFn` map form, not the positional `Vec<Value>` form.
- `Option<T>` = optional (absent → `None`); non-`Option` = required. Non-`Option` defaults (`port: u16 = 22`) explicitly out of v1.
- New `FuncKind` or flag on `HostRegistration` marking named-model (drives the runtime branch + RPN exclusion + `pure`+named rejection).

### A5. Metadata, introspection, docs
- `FuncParam` gains `optional: bool` (`native.rs:40-43`); flows through `meta_to_value` (`DESCRIBE`), `FUNCTIONS` (names only, unchanged), and docs-gen signatures (`NAME($REQ, [$OPT])` style in `command_ref.rs:216-236` + generated `function-reference.md.tmpl`).
- Existing exact-arity messages and tests (`commands.rs:1272-1331`) untouched — positional path byte-identical.

### A6. Tests (new)
- Parser unit: named syntax, any-order, duplicate rejection, `=` edge cases (regression: `FOO(a == b)` positional with `==` still parses), Display round-trip, named-in-RPN is a parse-time error with the exact needle.
- Macro: malformed declarations fail (non-scalar field, double-`Option`, `pure`+named) — no trybuild harness exists; plain `#[test]` + `cargo check` on fixture crates or `syn`-level unit tests in the macro crate.
- Integration: SSH module named calls (all five funcs), sparse-optional regression (`req` + later `opt2` with middle `opt1` omitted binds correctly), case-insensitivity (`bind=` binds `BIND`; `bind=1, BIND=2` errors as duplicate), error needles (missing required, unknown name with expected-list, positional-into-named, named-into-positional), `DESCRIBE` shows optional flags.
- Miri ignores with reasons where threads/sockets/fork appear.

### A7. Explicitly deferred
- User `FUNC` defs with named params/defaults (`FUNC F($a: INT = 1)`): same model applies later; v1 is host funcs only.
- Non-`Option` defaults, variadics, parse-time arg-name checking via `ModuleTable`.
- Migrating STD builtins (would break every script; separate decision).

## Part B: Stable host key (first consumer of Part A)

### B1. Surface
- `SSH_SERVE(bind=..., username=..., password=..., key_path?=...)` under the new named model (whole SSH module migrates per A0):
  - `key_path` empty/absent → today's behavior (fresh in-memory Ed25519 per call, zero trace).
  - `key_path` set → workspace-relative OpenSSH key file, **load-or-create**: parse existing (Ed25519 only, bail otherwise), else generate and create with `0600` at open time (never write-then-chmod; see B2).

### B2. Implementation notes
- `russh::keys::PrivateKey` IS `ssh_key::PrivateKey` (re-export) — `read_openssh_file` / `write_openssh_file` / `random` plug straight into `Config.keys`. No conversion code.
- Plugin gains an `oxdock-fs` dependency (workspace member, allowed): resolve the path against the workspace guard (`cx.cwd().root()` + `PathResolver`). New `oxdock-fs` helper (e.g. `write_private_file`) creates with `0600` **at open time** (`OpenOptionsExt::mode(0o600)` on Unix, `create_new` so concurrent first-starts don't truncate each other; on `AlreadyExists`, validate-then-read the existing file). Never write-then-`set_permissions_mode_unix`: that leaves key bytes under default umask between creation and chmod. The existing post-hoc `set_permissions_mode_unix` (`workspace_fs/io.rs`) stays for non-secret uses.
- Existing-file permission gate (Unix-only, like the existing mode tests): before reading an existing key file, assert `mode & 0o077 == 0` (i.e. `0600` or `0400`); otherwise bail `insecure permissions on host key file <path>: expected 0600, got 0<mode>`. A world-readable pre-existing key must fail loudly, never load silently. Non-Unix skips the check (Windows ACLs; noted follow-up).
- Secrets discipline unchanged: never log key material; file location is user opt-in; `Display`/`Debug` still redact.
- Clippy/deny: `ssh-key` already in the russh-closure allow-list; no new licenses expected (verify with `cargo deny`).

### B3. Tests (new)
- Load-existing: same key across restarts (client accepts without re-prompt; compare host-key fingerprints).
- Create: missing path generates a file, parses as OpenSSH Ed25519, mode `0600` (unix-only assertion); umask-exposure test (restrictive umask cannot widen the created file, since mode is set at open, not chmod-after).
- Wrong-type file bails with a clear needle; insecure existing permissions (`0644`) bail with the security needle (unix-only); empty `key_path` keeps ephemeral behavior (existing tests updated to named calls).
- Update: proto script + CLI tests to named `SSH_SERVE` (mechanical), Miri ignores as established.

## Verification (both parts)
`cargo test --workspace --tests`, `cargo clippy --workspace --all-targets`, `cargo fmt --all --check`, `cargo sort --workspace --check`, `cargo deny check`, `cargo run -p docs-gen` + commit regenerated outputs. Manual: proto script with `key_path`, restart server, reconnect without re-prompt.

## Risks
- Grammar `=` vs `==`: single-`=` in call parens is currently a hard parse error everywhere EXCEPT inside `==` comparisons, which is exactly why the `!"="` lookahead and the `FOO(a == b)` regression test are mandatory; note it in the grammar comment.
- `Value::map` handoff adds one `BTreeMap` alloc per named call (negligible next to step dispatch) and a new `HostRegistration` variant that `register_module` and any exhaustive matches must handle (compiler-guided).
- `oxdock!` macro + `oxdock_embed!` fixtures must handle the new AST shape (compile-checked by existing fixture crates).
- SSH module migration touches every SSH test/script (mechanical, contained).
- Scope creep into `FUNC` named params or STD migration: explicitly out; say no if asked mid-build.
