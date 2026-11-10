# Plan: #287 Enforce declared FUNC return types (hard, no legacy fallback)

Source: https://github.com/jzombie/rust-oxdock/issues/287
Plans dir (per `SCRIPT::PLAN_DIR`): `CWD/.opencode/plans/` — this file lives there, allowed by `agents.plan.permissions`.

## 1. Objective restated

- `FUNC NAME($p: TYPE, ...): RET { ... }` — return annotation mandatory. Missing `: TYPE` is a parse error.
- Every `RETURN <expr>` checks against declared `RET` statically, all paths. `VOID` is a real declaration type (new `TypeTag` word): `FUNC ... : VOID` means returns nothing — fallthrough and bare `RETURN` are the only legal exits, `RETURN <expr>` is a static error. Non-`VOID` `RET` requires an explicit `RETURN` of that type on every path; fallthrough and bare `RETURN` are static `MissingReturn`-family errors naming function. No `""`-admits-fallthrough carve-out: `STRING` functions `RETURN ""` explicitly.
- New `ASYNC FUNC` definition form: every path returns `HANDLE`. `ASYNC FUNC ...: HANDLE`, checker enforces `HANDLE` on every `RETURN` + fallthrough. Union returns on `ASYNC` must resolve to `HANDLE` on every path (i.e. effectively only `HANDLE` / `ANY`-that-is-`HANDLE`? Issue says union must resolve to `HANDLE` — interpret as each union member must be `HANDLE`-compatible; simplest: declared must be exactly `HANDLE`, `ANY` rejected for `ASYNC FUNC`).
- Static (parse/lower) time, before first step. No legacy fallback.
- `DESCRIBE`/registry render declared return; MCP `output_schema_for` uses it.
- Migrate entire suite: no unannotated `FUNC` remains in tests, fixtures, examples, prototypes, CI pipeline, docs.

Proposed canonical syntax (from issue):
```oxdock
FUNC DO_SOMETHING(): SOME_RETURN_TYPE { RETURN $something_with_that_return_type }
ASYNC FUNC FETCH(): HANDLE { RETURN $task }
```

> Addendum (HANDLE must carry payload — agreed, this is load-bearing for #287):
> - Today `HANDLE` is unparameterized: `TypeTag::Handle` leaf (`tag.rs:74`), `HandleValue(pub u64)` (`value.rs:166-168`), task entry holds `return_value: Option<Value>` + `returns_value: bool` with no tag (`state.rs:361-385`). Type is applied only at consumption (`LET $o: TYPE = AWAIT $t` coerces via `declare_var`/`coerce_value`, `state.rs:844-864` + `args.rs:96-184`; `TASK_POLL.result?: ANY`, `native.rs:3443-3448`).
> - Consequence: `LET $t: HANDLE = ASYNC { RETURN "hi" }` + `LET $i: INT = AWAIT $t` fails only at the `AWAIT` coercion, never at spawn; `ASYNC FUNC ... : HANDLE` as specced declares nothing about the payload. The fix is `HANDLE<T>`: `LET $t: HANDLE<STRING> = ASYNC { RETURN "hi" }`, `ASYNC FUNC FETCH(): HANDLE<STRING>`.
> - Shape (mirrors `LIST<T>`): new `TypeTag::HandleOf(Arc<TypeTag>)` (keep bare `Handle` = `HANDLE<ANY>` for back-compat, or reject bare `HANDLE` as declaration like bare `LIST`/`MAP` at `typing.rs:166-175` — decide in implementation; recommend bare = `ANY` payload to keep migration tractable). Touch points: `tag.rs` enum + `name()` + `render_structural` (`LIST<..>` precedent, `:236`) + `check_value_at` (`:500`, handle arm stays id-check — payload is phantom at the handle value itself) + `parse_spelling` (already parses `HANDLE<X>`, `:409`); `typing.rs:262-316` `build_generic` (allow `HANDLE` with exactly one bare arg, today bails at `:269-271`); `coerce_value` `args.rs:96-184` + `coerce_scalar` (new tag must appear in the explicit-tag lists); MCP `schema.rs` `Handle` projection + `tools.rs` `output_schema_for`/`map_json_args` (`args.rs:1146-1152,1483-1488`); `tags_equal`, `widen` (`static_check.rs:2651` opaque-handles-to-`ANY`), `DESCRIBE` rendering.
> - Enforcement sites (both currently missing): `AssignAsync` arm (`static_check.rs:1893-1919`) walks body with `RetCtx::AsyncValue` then discards `inner.types` — must unify body `RETURN`s and check against the `HANDLE` payload `T`. `AwaitCapture` arm (`:1862-1876`) resolves `out_type` but never reads the handle var's tag — must load `task_var: HANDLE<T>` from env and require `out_type` compatible with `T` (plus today's coercion at runtime). Same for `TASK_POLL.result` (`native.rs:3443-3448` `ANY` → should project `T`).
> - `ASYNC FUNC` then declares `: HANDLE<T>`, not bare `: HANDLE`; every `RETURN` yields `HANDLE<T>` whose payload `T` matches the task body, and every `AWAIT` of it checks `T` against the capture site. Without this, #287's `ASYNC FUNC` guarantee is vacuous.

## 2. Current state (verified)

### 2.1 FUNC has no return slot
- Syntax doc: `crates/oxdock-parser/src/commands.rs:3623` — `FUNC NAME([$param: TYPE, ...]) { <commands> }`.
- Header parser: `crates/oxdock-parser/src/parser.rs:4219-4350` `parse_func_at()`:
  - name = `toks[pos+1]` UPPERCASE (`4242-4249`), paren group at `pos+2` (`4253-4261`), params loop `$name : <type-shape>` via `parse_type_shape` + `render_type_region` + `canonicalize_spelling` (`4263-4304`), block required (`4306-4317`). No `:` + return parsing after `)`.
  - Effects: reserved-shadow (`4320-4324`), `ctx.declare_func` same-scope dup (`4325-4329`), param dupes (`4330-4337`), body `parse_body_steps` (`4342`), builds `StepKind::FuncDef { name, params, body, docs: None }` (`4343-4348`).
- AST: `crates/oxdock-parser/src/commands.rs:1152` `FuncDef { name, params: Vec<(String,String)>, body, docs }` — no `return_type`. Display round-trip `commands.rs:4528-4551` renders without return.
- Runtime registry form: `crates/oxdock-core/src/exec/native.rs:223-235` `FuncDefData { params: Vec<(String,TypeTag)>, body, module }` — no return. `define_script` (`native.rs:569-639`) sets `meta.returns: None` (`621`), `summary: "DSL-defined function."`.
- `define_func` handler: `crates/oxdock-core/src/exec/handlers.rs:2468-2489` resolves param strings via `cx.state.resolve_tag`, calls `define_script(name, &resolved, body, owner)`.

### 2.2 RETURN + static check today (inferred, not declared)
- Parse: `parser.rs:6840-6865` `parse_return_body()` — bare `RETURN` → `Return { expr: Literal("") }` (`6850-6857`), else `parse_expr_checked`.
- Static: `crates/oxdock-core/src/exec/static_check.rs:1493-1560` `check_function_body_probed()`:
  - `has_return && falls_through` → bail `function '{key}' may fall through with no RETURN on some path` (`1543-1547`).
  - `!has_return` → `Ok(StaticTy::Void)` (`1549-1551`); else unify all terminal RETURN types via `unify` (`1552-1559`).
  - `StaticTy::{Ty, Any, Void}` (`94-95`). Call-site `VOID → STRING` mapping in `check_call` (`2515-2519`), mirroring runtime `Flow::Done => Ok("")`.
- Runtime: `call_func_value` doc `handlers.rs:2491-2498` — fallthrough yields `""`.

### 2.3 Type system (reuse as-is)
- `crates/oxdock-parser/src/tag.rs:66-95` `TypeTag::{String,Int,Float,Bool,List,Map,Pipe,Handle,Duration,Path,Semaphore,Permit,Any,Custom,ListOf,Record,MapOf,Union}`.
- Generics: `LIST<...>`→`ListOf`, `MAP<ANY>`→`MapOf`, `MAP<k: T>`→`Record` (`tag.rs:234-255`), depth 8. Unions `A|B` via `TypeTag::union` (`141-149`), `check_value_at` tries members (`576+`).
- `render_structural` (`tag.rs:234-248`) + `canonicalize_spelling` (`tag.rs:290-325`, called from `parser.rs:6191-6225`) — reuse for return spelling.
- `resolve_tag` choke used at `define_func` (`handlers.rs:2479`) — same path should resolve return tag.

### 2.4 DESCRIBE / MCP (currently empty for scripts)
- `STD::DESCRIBE` `native.rs:1703-1780` → `meta_to_value` `native.rs:3532-3623`: `returns: meta.returns.map(render_structural).unwrap_or_default()` (`3608-3616`) — all script FUNCs render `""`.
- MCP: `crates/oxdock-mcp/src/schema.rs:28-115` `schema_for_tag`, `tools.rs:96-111` `output_schema_for` (`returns:None → {}`), `tools.rs:123-180` `input_schema_for`. Server build `server.rs:801-846`, tests `crates/oxdock-mcp/tests/tools.rs`, `session.rs`.

### 2.5 ASYNC today (no ASYNC FUNC)
- `StepKind::{AsyncBlock, AssignAsync}` vs `FuncDef` are disjoint (`commands.rs:1143-1144,1152`). `ASYNC FUNC` string has zero hits. `#[oxdock_func]` rejects async fns (`crates/oxdock-func-macro/src/lib.rs:1316`).

### 2.6 Migration blast radius (from repo-wide FUNC scan)
- MCP demo: `crates/oxdock-mcp/demo/tools.oxdock:6,10,15,19,37,47` (`ADD, GREET, APP_NAME, COUNT, WHEREAMI, HELP`), export `:65`.
- Logic fixtures: `crates/oxdock-logic-tests/fixtures/commands/ast_commands/cases/*/script.oxdock` — `func_def_call, func_return, func_list_describe, call_statement, func_shadow_native, func_let_capture, func_scope_isolation, func_async, func_nested_shadow, func_duplicate_same_scope, func_pipes, isolated` + corpus `tests/oxdock_macro_corpus.rs:335,651,656,661,666,717`, `service_enforcement.rs:217`, `perf_regression.rs:59`.
- Core: `crates/oxdock-core/tests/integration/commands.rs` (~20 sites), `custom_types.rs:131,143,173,184,258,314,318`, `src/exec/tests.rs` (~14 sites), `src/lib.rs:1393`, `handlers.rs:2464`, `native.rs:1964` doc examples.
- Parser: `tests/integration/macro_input_parens.rs:170,218,260,264,292,295,329-362`, `import.rs:577,598,621`, `type_syntax.rs, error_kinds.rs, remote_block.rs`.
- Examples/pipelines: `oxdock/examples/guess.oxfile:9,23`, `hangman.oxfile:10,33`, `scripts/bench/callstr.oxdock:2, bench.oxdock:24`, `oxdock-render/src/render.oxfile:12,196,215,228`, `stages.oxfile:8`, `lifecycle/*.oxfile`, `install.oxfile:14,24`, `.github/scripts/helpers/ci-helpers.oxfile:20,27,37,62`, `gha-miri-*.oxfile`, templates `crates/oxdock-mcp/.oxdock/template/fragments/overview.md.tmpl:58,63,73`, `.oxdock/template/installer/install.oxfile.tmpl:14,24`.

## 3. Proposed implementation

### Step A — Parser: mandatory `: RET` + `ASYNC FUNC` shape
File: `crates/oxdock-parser/src/parser.rs` (`parse_func_at`, new `parse_async_func_at` or prefix branch).
- After paren group (`p` at `pos+3`), require gap-optional `:` then `parse_type_shape` + `render_type_region` (same as params). Missing colon → `Err(fail("FUNC {name} requires a return type `: TYPE` ..."))`. Bad shape → same error (not silent `None` miss; this is the hard break — but only after FUNC name+parens matched, so non-FUNC statements still miss to generic).
- Canonicalize via existing `canonicalize_spelling`; store `return_type: String` (spelling) alongside params.
- `ASYNC FUNC`: new header. Token lookahead: `ASYNC` word at `pos`, `FUNC` at `pos+1`, then same as above. Options: (1) separate `StepKind::AsyncFuncDef` variant, or (2) `FuncDef { is_async: bool }`. Prefer (2) — one struct, one registry path, `is_async` flag drives checker. Parse-time: if `is_async` and declared spelling != `HANDLE` (after canonicalize; decide `ANY` policy — recommend reject `ANY`/unions for ASYNC FUNC per issue "must resolve to HANDLE on every path"), fail naming function.
- Bare `RETURN` (→ `""` literal) stays parsed; checker decides if `""` checks against declared type.
- Update `syntax:` string in `commands.rs:3623` + description (`3625-3645`) to `FUNC NAME(...): RET { ... }` / `ASYNC FUNC NAME(...): HANDLE { ... }`.

### Step B — AST + display + docs sidecar
Files: `crates/oxdock-parser/src/commands.rs:1152,4528-4551`, `parser.rs:8050-8128` (`ParsedScript`, `collect_func_docs`, `stamp_func_docs`).
- `FuncDef { name, params, return_type: String, body, docs, is_async: bool }` (or `AsyncFuncDef` if chosen). Update constructor at `parser.rs:4343-4348`, display renderer, `define_steps!` variant list.
- Check other `FuncDef` constructors/match sites: `grep FuncDef` across parser + core handlers + static_check + macro corpus. Update exhaustively.
- `func_docs` sidecar unchanged (keyed by name), but `stamp_func_docs` must carry through new fields.

### Step C — Registry + define path
Files: `crates/oxdock-core/src/exec/native.rs:223-235,569-639`, `handlers.rs:2468-2489`.
- `FuncDefData { params, return_type: TypeTag, return_spelling: String, body, module, is_async: bool }`.
- `define_script(name, params, return_tag, body, module, is_async)` — resolve return via `cx.state.resolve_tag(return_spelling)` in `define_func` (same choke as params, unknown names fail naming function + known types). Set `meta.returns = Some(return_tag)`.
- `define_aliased_in` (`native.rs:650-730`) same treatment for `IMPORT`ed FUNCs.
- Host `FuncMeta.returns: Option<TypeTag>` already exists — no change needed there.

### Step D — Static checker: declared-type enforcement
File: `crates/oxdock-core/src/exec/static_check.rs` (`check_function_body_probed:1493-1560`, `FuncInfo` construction, `Return` arm `:1675+`, `unify`).
- Thread declared `TypeTag` into `FuncInfo` (find construction sites — `grep FuncInfo`).
- For each terminal `RETURN` type `t`: check `t` against declared `d`. Reuse `check_value_at`-style compatibility or `unify(t, d)`? Recommendation: strict `is_compatible(t, d)` = `d==Any && ok` (except ASYNC), else `unify(t,d)` must succeed AND equal `d` (so `RETURN 1` against `INT|STRING` fails? No — `1: INT` is a member of union, should pass. So rule: `t` must be a subtype/member of `d`: if `d` is `Union`, `t` must equal one member (or `Any`? no); if `d` is `Any`, any `t` passes; if `d` is generic `ListOf/MapOf/Record`, use existing `check_value_at` semantics at type level. Simplest correct: keep collecting unified `u = unify(all returns)`, then require `u` compatible-with `d` via same member rule, PLUS per-site error naming function + expected + site line. Per-site check gives better errors than unified-only, so do per-RETURN check with `scope.line/col` in message: `function '{key}' declares `{d}`, but RETURN at line {l} yields `{t}``.
- Fallthrough: `out.has_return && out.falls_through` currently always bails (`1543-1547`). New rule: bail unless declared type "admits" fallthrough. Define admits = declared is `STRING`? No — fallthrough yields `""` at runtime, so admits iff `"" : d` checks (i.e. `STRING`, `STRING|...`, `ANY`). `!has_return` (`1549-1551` currently `Void`) → same: only ok if admits-empty, else `MissingReturn`-family bail naming function + declared type. Remove `StaticTy::Void` → call-site `VOID→STRING` mapping (`2515-2519`) becomes dead for script FUNCs (keep for host void? check if any host has `returns:None` — `INSPECT` has `Some(Map)`; `MCP_STATUS` has `None` — keep mapping until all hosts annotated, then delete).
- `ASYNC FUNC`: after generic checks, additionally require every `RETURN` type == `HANDLE` (and fallthrough rejected always — `""` is not `HANDLE`). Declared is `HANDLE` by parse, so generic check already covers it; the extra value is a clearer error message naming `ASYNC FUNC` + `HANDLE`. Also reject `BREAK`/`CONTINUE` already covered.
- Error kinds: add `ParseErrorKind`/static variants if repo uses them for FUNC (check `ParseErrorKind::Validation` usage at `parser.rs:4236`); keep messages greppable: `function '{key}'`, `expected`, `RETURN`.

### Step E — Runtime (minimal)
File: `handlers.rs:2491+` `call_func_value`.
- No behavior change needed if static is sound: `RETURN` yields value, fallthrough yields `""` (now only reachable when declared admits `""`). Optionally add debug assertion that runtime value checks against `return_tag` (cheap `check_value_at`) — recommend behind `debug_assert!` only, to avoid perf hit.

### Step F — DESCRIBE / MCP / docs reference
Files: `native.rs:3532-3623` (`meta_to_value`), `crates/oxdock-mcp/src/schema.rs`, `tools.rs:96-111`, `server.rs`, […]/`plugin_docs()` + `render.json` generated reference.
- `meta_to_value` already renders `Some` — script FUNCs now always `Some`, so `DESCRIBE("SCRIPT::X").returns` becomes e.g. `"STRING"`, `"HANDLE"`, `"INT|STRING"`. Update `func_list_describe` fixture (`script.oxdock:9-12`) + `native.rs:3850-3948`, `tests.rs:5206-5223` expectations.
- MCP `output_schema_for`: `returns:None → {}` branch becomes unreachable for scripts; keep for `MCP_STATUS` until annotated. Verify `HANDLE` projection (`schema.rs`) covers `ASYNC FUNC` tools.
- Generated function reference (`plugin_docs()`, `render.json`, `.oxdock/template/...`): re-run `cargo run -p oxdock-render` after changing summaries if any; do NOT hand-edit READMEs (per AGENTS.md).

### Step G — Migration (mechanical, large)
- Script to list every `FUNC` lacking `:` after `)`: `grep -rn "^[[:space:]#]*\(ASYNC \)\?FUNC " --include=*.oxdock --include=*.oxfile --include=*.rs --include=*.md.tmpl .` then annotate each with correct `: TYPE` (infer from unified RETURN types / call-site `LET $x: TYPE` / docs).
- Order: parser tests → logic fixtures → core tests → mcp demo/tests → examples/bench → render pipeline oxfiles → CI helpers → templates → docs fences (every `oxdock` fence in templates executes under `docs_conformance` — must stay compiling).
- Each migrated FUNC needs its `RETURN`s to check; bare `RETURN` only where declared admits `""`.
- Textbook cases to verify first: `func_return/PICK`, `func_def_call/GREET`, `func_async/WORK` (likely becomes `ASYNC FUNC WORK(): HANDLE`), `DOUBLE` (INT), `MAKE` (STRING?), `POLL_WRAP(HANDLE)`.

### Step H — Tests (acceptance mapping)
- `FUNC` without `: TYPE` fails at parse naming fix (new test in `crates/oxdock-parser/tests/integration/error_kinds.rs` or `type_syntax.rs`).
- Wrong-typed `RETURN` fails statically naming function + expected + site (new `static_check` test + fixture case).
- Fallthrough with non-admitting return fails (existing message at `static_check.rs:1543` extended with declared type).
- `ASYNC FUNC` with non-`HANDLE` path fails (parse + static tests).
- Generic/union returns check exactly (new cases: `LIST<MAP<ANY>>`, `INT|STRING`, nested).
- Suite green: `cargo test --workspace --tests` + `docs_conformance` + `cargo clippy` (no new allows; `_`-prefix bans per AGENTS.md).

## 4. Open questions (decide before coding)
1. `ASYNC FUNC` surface: prefix form `ASYNC FUNC F(): HANDLE` (recommended, mirrors `ASYNC` block keyword) vs modifier `FUNC F() ASYNC: HANDLE`? Issue says "`ASYNC FUNC` definition form" — go prefix.
2. Fallthrough-admits definition: exactly "empty-string checks against declared" (`STRING`, unions containing `STRING`, `ANY`)? Or also `VOID`? Recommend former; `VOID` dies with this change.
3. Bare `RETURN` = `RETURN ""`: keep desugar, let checker accept/reject per declared type. Agreed?
4. `ANY` on `ASYNC FUNC`: issue says union must resolve to `HANDLE` on every path — recommend `ASYNC FUNC` declared type must be exactly `HANDLE` (reject `ANY`, reject unions even containing `HANDLE`). Simpler, matches "must return a handle".
5. `IMPORT`ed FUNCs (`ALIAS::NAME`): return travels with definition; `define_aliased_in` carries tag. No separate annotation at import site. Agreed?
6. Host functions with `returns: None` (`MCP_STATUS`, `INSPECT` has `Some`): annotate or keep `None→{}` escape? Keep escape until follow-up; scripts are the enforcement target.

## 5. Suggested work order
1. Parser + AST + display (Step A–B) with 3–4 focused parse tests.
2. Registry + `FuncInfo` plumbing (Step C + half of D).
3. Checker rules + errors (Step D) with fixture cases per acceptance bullet.
4. DESCRIBE/MCP expectations update (Step F).
5. Mechanical migration (Step G) — largest diff, do last, keep green throughout.
6. Full `cargo test --workspace --tests`, `oxdock-render` regen, clippy.

## 7. Void functions and "empty HANDLE" (verdict: real VOID, no STRING-for-nothing)

- `VOID` is a real declaration type (new `TypeTag::Void`, `name() == "VOID"`, `builtin("VOID")`). Legal only as a `FUNC` return annotation (and `HANDLE` payload, below). `LET $x: VOID`, params of type `VOID`, and record fields of type `VOID` are rejected, same family as the bare-`LIST`/`MAP` declaration rejections (`typing.rs:166-175`).
- `FUNC F(...): VOID`: every exit must be fallthrough, bare `RETURN`, or `EXIT`. Any `RETURN <expr>` is a static error naming function + site. Bare `RETURN` is the void exit (not `""`); the checker accepts it in `VOID` and rejects it elsewhere.
- `FUNC F(...): T` (non-`VOID`, incl. `STRING`): every path must `RETURN` a `T`. Fallthrough and bare `RETURN` are `MissingReturn` errors naming function + declared + site. A `STRING` procedure ends with an explicit `RETURN ""`. Nothing is inferred from `""`.
- Call-site: the `VOID→STRING` mapping (`static_check.rs:2515-2519`) is deleted. A `VOID` function as a statement is fine; used as a value (`LET $x = F()`, argument, push operand) is a static error naming the function. Runtime keeps yielding `""` on the wire for the void exit (no new `Value` variant) but the checker makes it unobservable.
- `!has_return` (`static_check.rs:1549-1551`, today `StaticTy::Void`): with mandatory annotations this is the `VOID` case. Body with no `RETURN` at all is legal iff declared `VOID`, else `MissingReturn`. Partial-`RETURN` + fallthrough (`:1543-1547`) keeps its error, extended with the declared type.
- No `VOID` values exist: `check_value(Void, _)` fails for every word; `coerce_value` (`args.rs:96-184`) gains no `VOID` arm. Coercion into `VOID` is unreachable because the call-site rule rejects it statically.
- "Empty HANDLE": no such value (`HandleValue(pub u64)`, `value.rs:166-168`). Void tasks are `HANDLE<VOID>`: `LET $t: HANDLE<VOID> = ASYNC { ...no RETURN... }`, consumed only by bare `AWAIT $t` (join). `LET $o = AWAIT $t` on a `HANDLE<VOID>` is a static error. This kills the old silent-`0` rule (`handlers.rs:4673-4679`) for typed code. Bare `ASYNC { ... }` (`AsyncBlock`, `RetCtx::AsyncStmt`) is unchanged: no handle, `RETURN` banned.
- `ASYNC FUNC ... : HANDLE<T>` (with `HANDLE<T>` per §1 addendum): every path must `RETURN` a real handle; fallthrough/bare-`RETURN` never admits. `T = VOID` is allowed and means the spawned task itself is void.
- Hard rule (no exceptions): every `ASYNC` position is `HANDLE`-headed. `ASYNC FUNC F(): VOID` is a parse-time error, always. `LET $x: <non-HANDLE> = ASYNC ...` (including `VOID`, `INT`, `STRING`) is a static error naming the variable and the offending decl; today this only fails late at `declare_var`/`coerce_value` (`handlers.rs:4427-4429`) — the checker must reject it up front in the `AssignAsync` arm (`static_check.rs:1893-1919`) by requiring the resolved decl to be `Handle`/`HandleOf`. Bare `HANDLE` continues to mean `HANDLE<ANY>` (back-compat); `HANDLE<VOID>` is the void-task spelling.

## 8. Verification
- `cargo test --workspace --tests`
- `cargo clippy` (workspace lints deny disallowed macros/methods/types; localized allows only)
- `cargo run -p oxdock-render` + commit regenerated READMEs
- New tests pin each of the 6 acceptance boxes.
