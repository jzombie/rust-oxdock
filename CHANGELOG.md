# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/) and this project adheres to
 (or is loosely based on) Semantic Versioning.

## [0.18.1-alpha] - 2026-09-24

## Changed

- Improve docs.

## [0.18.0-alpha] - 2026-09-23

### Added

- `WORKSPACE CACHE` and `WORKSPACE SYSTEM` roots (#163): `CACHE` is a persistent per-project directory shared across runs, resolved through the `cache-manager` crate with OS-native per-user roots and created on first use with no eviction by default; `SYSTEM` grants full filesystem access with per-path filesystem anchors so every drive and UNC share resolves. Both participate in `WORKSPACE` switch, scope-revert, and fork semantics like the other roots. Cache identity resolves with no disk reads as explicit builder argument, `OXDOCK_CACHE_APP`, runtime `CARGO_PKG_NAME`, running binary name, then `"oxdock"`; `OXDOCK_CACHE_DIR` pins an exact directory (also honored by the doc-fence harness).
- `COPY --from-workspace SNAPSHOT|LOCAL|CACHE|SYSTEM`: selects the copy source root explicitly. `SNAPSHOT` requires a materialized snapshot, `LOCAL` keeps the former workspace-root semantics, `CACHE` ensures the persistent directory first, and `SYSTEM` resolves absolute sources without confinement.
- `SYMLINK --from-workspace SNAPSHOT|LOCAL|CACHE|SYSTEM`: selects the symlink source root explicitly, mirroring `COPY --from-workspace`. Absent the flag, the build-context default applies as before.
- `WORKSPACE CACHE --local`: keeps the persistent cache in `<project>/.cache/workspace` instead of the OS per-user cache. `OXDOCK_CACHE_DIR` stays scoped to the OS flavor; the local flavor lives and dies with the project tree and is never evicted.
- Docker destination semantics for `COPY` and `SYMLINK`: a file copied onto a directory (an existing one, or a trailing-slash spell like `out/`) is duplicated inside it under its own basename (the source is never moved); a directory source duplicates its contents; any other destination path is created holding the copied bytes. `SYMLINK` onto a directory places the link under the source basename instead of failing as "already exists".
- Shared `oxdock_fs::env` module: every `OXDOCK_`/`CARGO_` runtime environment name the workspace reads or writes lives there as a `pub const`, replacing hardcoded duplicates (`env!` compile-time macros keep their literals).

### Changed

- [breaking] Scripts are affected: `COPY --from-current-workspace` is replaced by `COPY --from-workspace LOCAL` with no alias.
- [breaking] Scripts are affected: `WORKSPACE` targets are uppercase-only. Lowercase spellings are rejected, the undocumented `A`/`B` aliases are removed, and `ArgType::OneOf` validation is exact match.
- COPY sources re-validate against the root they resolved under, so cross-root copies (a `CACHE`, `SYSTEM`, or `SNAPSHOT` source under a different selection) no longer fail at copy time; `SYSTEM` sources re-wrap lexically with no confinement.

### Fixed

- DSL `--flag=value` single-token form: flags like `COPY --from-workspace=CACHE` and `ASSERT_EQ --hash=<digest>` now parse. Tokens starting with `--` no longer match `KEY=value` assignment, so the value arrives inline instead of traveling as the next whitespace-separated token.
- `copy_from_workspace_outside_escape` fixtures remove their system-temp probe files after asserting instead of littering them.
- `COPY`/`SYMLINK` onto a directory destination (e.g. `COPY file .`) no longer fails with a false workspace-escape error: parent directories defer to the creation check instead of a confining read check. `SYMLINK` sources also re-validate against the root they resolved under, matching `COPY`.

### Dependencies

- Add `cache-manager` 0.4.1 with `os-cache-dir` (persistent project cache roots; pulls `directories` 6).

## [0.17.0-alpha] - 2026-09-21

### Added

- `NET` host plugin (new `oxdock-net-plugin` crate, registered by the CLI in every build): scripts `IMPORT [NET]` and pump TCP through explicit pipes. `NET_LISTEN(endpoint)` binds a logical endpoint and returns a MAP with `listener` (`NET_LISTENER`), `addr`, and `virtual` echo; `NET_ACCEPT` accepts one connection into pipes under `ASYNC`; `NET_CONNECT(endpoint, ...)` dials one connection through pipes under `ASYNC`; `NET_CLOSE` shuts down. Pumps are always full duplex over caller provided pipes.
- `SSH` host plugin (new `oxdock-ssh-plugin` crate, CLI `--features ssh`): scripts `IMPORT [SSH]` for an ephemeral user space server and client. `SSH_SERVE(endpoint, {username, password, key_path?})` returns a MAP with `server` (`SSH_SERVER`), `addr`, credentials, and `virtual` echo; `SSH_DEQUEUE($server)` under `ASYNC` returns `{session (SSH_SESSION), command, username, addr}`; `SSH_PUMP_CHANNEL($session, $in, $out)`, `SSH_CONNECT` / `SSH_PUMP`, and `SSH_CLOSE` bridge bytes through pipes; `SSH_PTY_RUN($session, argv, rows, cols, in, out)` runs under `ASYNC` with per session terminal sizes and the script environment layered over the host environment (relay per session identity with `ENV SSH_USER/SSH_CLIENT/SSH_SERVER/SSH_COMMAND` from the dequeue map; `scripts/proto.echo-listen.oxdock` demonstrates the semaphore reservation pattern). `key_path` loads or creates a host key with `0600` (a leading `/` anchors to the workspace root, blank keeps the ephemeral key). Memory services bail since SSH needs a TCP socket. Hosts gain `StepCtx::env_snapshot` for the same layering.
- Virtual service endpoints for `NET` / `SSH`: scripts declare logical endpoints (`NET_LISTEN("2251")`, `SSH_SERVE("demo-proxy")`) while the host runner maps them to physical interfaces. New CLI flags `--listen <addr>` (expose a logical port, the only place wildcards may appear), `-p <outer:inner>` (map an outer port to an inner port or name, outer `0` takes an ephemeral port), and `--offline` (open no sockets at all; every dial bails before DNS). Unmapped bare ports bind loopback, unmapped names open in-process memory rendezvous with zero sockets (`NET_CONNECT` joins them from either side, client-before-server included), and pre-bound sockets are claimed (shared backlog) rather than taken so close-and-rebind loops keep working. Memory session queues cap at 64 pending per service. Both result MAPs gain a `virtual` echo key; `addr` reports the physical bind, or the virtual echo when socketless.
- New `oxdock-pipe` crate (#159): anonymous pipe backends (spillable script buffers, take-once OS kernel pairs, lazily-materializing owned handle slots), relocated out of `oxdock-core` so `PIPE` values can own handles without a dependency cycle. `SharedInput` / `SharedOutput` and the take-once OS halves move there too, re-exported from `oxdock-process` with no API change.
- New `LIST_APPEND $list <item>` command: appends to a `LIST` binding in place through the copy-on-write gate (sole owners mutate with no copy, shared buffers detach so aliases keep their contents). Multi-word command names now group by domain prefix (`CATEGORY_ACTION`); new categories register in `COMMAND_CATEGORIES`.
- `SEMAPHORE` admission control: `SEMAPHORE_NEW(max)` builds a counting semaphore, `SEMAPHORE_TRY_ACQUIRE($sem)` answers `{held, permit?}` without ever waiting (misses carry no `permit` key, so branch on `$m.held`), and `SEMAPHORE_AVAILABLE($sem)` reads free permits for observability only (audit lines, `active = max - free`). `PERMIT` words release exactly once on last drop, so permits bound in an iteration scope (or held by an `ASYNC` worker) return through ordinary frame teardown on every exit path with no DSL cleanup code. No blocking acquire exists, so the primitive adds no new wait surface.
- `STD::IS_TERMINAL(stream)`: reports whether `stdin`, `stdout`, or `stderr` is a terminal for the step as currently bound. Anything diverted reports false without touching host handles: `WITH_IO` pipe bindings, `LET` capture sinks, staged runner sinks, and materialized stdin streams. Only a directly inherited fd falls back to the process check. The name matches exactly with no case folding; anything else bails.
- Inline `LET` blocks: `LET $x: TYPE = { ... }` evaluates a block as an expression. `RETURN` ends the nearest function, `ASYNC` task, or inline block (bare `RETURN` yields `""`; falling off the end yields `""`); blocks nest.
- Multiline bracket interiors in scripts: call arguments, list literals, map literals, and parenthesized expressions may span lines with one entry per line, with `//`, `/* */`, and trailing `#` comments between entries. Statement structure and operators stay single line.

### Changed

- CLI argument parsing moves to `lexopt`: `--flag=value` equals forms, attached short values (`-p2222:2251`), and the `--` positional separator now parse; every flag, error message, and the usage text keep their existing shapes.
- [breaking] Pipes are anonymous handles (#159): `LET $p: PIPE` mints a fresh backend owned through the variable (no initializers, no shared names), `WITH_IO [stdin=$p]` / `[stdout=$p]` bind it, `LET $q: PIPE = $p` shares it, and a `$var` holding a `PIPE` in assertion position peeks its backend bytes. The backend materializes lazily on first binding: script pipes by default, zero-copy OS kernel pairs for pure single-`RUN` background pipelines. Mismatched later bindings adapt instead of failing. A second take on one OS end is a step-numbered error. `INSPECT($p)` reports `unbound` before first binding. Host functions gain pipe byte access on the step context (`pipe_reader` / `pipe_writer` / `new_pipe` / `close_pipe`) plus `PipeStream` slice-based `Read`/`Write` adapters.
- [breaking] Host Rust API only, scripts are unaffected (#159): pipe plumbing is handle-keyed instead of name-keyed. `Value::pipe(name)` / `Value::pipe_anonymous` are `Value::pipe_fresh()` / `Value::pipe_handle(handle)`; `as_pipe_name()` is `as_pipe_handle()`; `ExecIo::insert_*_pipe`, `input_pipe`, `stdin_pipe_inner`, and `pipe_backend` are gone and `ensure_pipe_for` is `ensure_handle`, with `resolve_stdin` / `resolve_stdout` / `resolve_stderr`, `peek_pipe_content`, `inspect_pipe`, and `pin_keeper` all taking `&PipeHandle`; `StepCtx::out_pipe_name` is `out_pipe` / `stdin_pipe` backends; `StreamHandle::Inherit`, `AssertTarget::Pipe`, and `PipeTarget::Name` are deleted.
- [breaking] `ASYNC` / `AWAIT` value semantics: a task publishes a value with an explicit `RETURN`, and `LET $out: TYPE = AWAIT $t` binds that value (or `INT` 0 when the task succeeded without one). Task output streams are live during the run; joining binds nothing by itself and no longer captures stdout. Add `RETURN <expr>` to the task body to yield a value.
- `FUNC` reference documents the statement-call form (`GREET("bex")` discards the value); the internal `bare_call_statement` grammar rule is renamed to `call_statement` with no syntax change.
- READMEs compare Rust host embedding against embedding Rust in Python.

### Fixed

- `ASYNC` task failures preserve the full causal chain across the task boundary instead of dropping every `Caused by` layer.

### Dependencies

- Add `bytes` 1 (SSH byte buffers; lock 1.12.1).
- Add `lexopt` 0.3.2 (CLI argument parsing).
- Add `portable-pty` 0.9.0 (local PTY runner for `SSH_PTY_RUN`).
- Add `rand` 0.10 (ephemeral SSH host keys; lock 0.10.2 alongside the pre-existing transitive 0.9.5).
- Add `russh` 0.63.3 with `aws-lc-rs` (ephemeral user space SSH server and client).
- Add `tokio` 1 with `rt`, `net`, `sync`, `time`, `io-util` (SSH runtime; lock 1.53.1).

## [0.16.0-alpha] - 2026-09-17

### Added

- Unified function registry with `#[oxdock_func]` host export macros (#146): all functions (DSL `FUNC`, natives, host-registered) invoke as `MODULE::NAME(...)` as statement and expression through `FunctionRegistry` lookups with depth and arity gates before any argument evaluates; `IMPORT [MODULE]` brings a module's names into bare-call scope for the rest of the enclosing block. New `oxdock-func-macro` crate derives a registration marker (`UpperCamelCase` of the function name) implementing `OxDockFn` from Rust signatures and doc comments (`#[oxdock_func]` / `#[oxdock_func(pure)]`); every builtin dogfoods it. `FUNCTIONS()` lists qualified names and `DESCRIBE("MODULE::NAME")` introspects the registry from the DSL (bare names fail closed, except `INSPECT`), and `FuncKind` distinguishes `Script` / `HostCtx` / `HostPure` (both host kinds render as `host`). `FuncMeta` records the owning `module` for every entry.
- Engine facade for host extensions (#146): `Engine::new()` plus `register_type::<T>()`, `register_module(HostModule { name, funcs, types })`, and `run_script` / `run_steps` encapsulate filesystem, process, and IO plumbing. `Extending OxDock from Rust` in the READMEs builds one complete `TAG` extension and runs a script against it, with every example executing as a doctest.
- Extensible type system on pointer vtables (#146): every DSL value is a 128-bit word (a `&'static TypeDescriptor` plus 64-bit payload) with zero-allocation inline storage for `Copy` scalars and thin-pointer boxes for heap types, and no registry of any kind on the lifecycle path. New `#[oxdock_type]` macro (plus `inline` mode) implements `OxDockType` on the payload struct with a canonical descriptor singleton; hosts mint with `Value::mint_heap` / `mint_inline` and read back with id-checked `read_heap` / `read_inline`. Per-state name directories back declarations, `TYPES()`, and `TYPE_DESCRIBE()`.
- Static function reference in generated docs (#146): docs-gen renders every `#[oxdock_func]` entry (qualified name, signature, evaluation contexts, summary, docs) into a `Functions` section staged in the workspace README, the `oxdock` reference, and the docs.rs crate docs, alongside the existing command and value-type references. Pure functions and `rpn`-opted-in stateful functions (`GLOB`, `LOAD_TOML`, `LOAD_JSON`) run on the compiled math path; everything else is AST-only, and `DESCRIBE()` reports the same flag per function.
- `modules:` prefix for the `oxdock!` macro: scripts calling host functions declare their modules up front (`modules: [DEMO],`), which the macro treats as opaque (membership checked at runtime); `STD`/`SCRIPT`-only scripts omit it.
- Shared container heaps with copy-on-write (#152): `LIST` and `MAP` words now co-own a reference-counted buffer instead of an exclusive box, so cloning is an O(1) strong-count bump with no allocation and no 128-bit word layout change. New `#[oxdock_type(shared)]` mode derives the shared vtable for host container types; hosts mint with `Value::mint_heap_shared` and read back with the existing `read_heap`. Every descriptor gains an `unshare` copy-on-write gate, and `Value::read_heap_mut` (plus `as_list_mut` / `as_map_mut`) is the only sound mutable path: exclusive heaps hand out their box, shared heaps detach (clone the buffer) when clones exist and mutate in place otherwise, and inline descriptors panic. The lifecycle stays Miri-verified, including detach-on-write, in-place-when-unique, and sibling isolation.

### Changed

- [breaking] `CALL` removed with no alias (#146): qualified `MODULE::NAME(...)` statements replace `CALL NAME(...)` (bare `NAME(...)` needs `IMPORT [MODULE]` or an in-script `FUNC`); `LET $x: T = FOO(...)` stays an expression assignment under the same rules. The call head and `(` must be contiguous (`ECHO (1 + 2)` stays an instruction). Same-scope `FUNC` redefinition and shadowing a reserved (native/host) name fail at parse time at the definition line, with a runtime guard for dynamic redefinitions. `FUNC` names resolve from their definition line (recursion works, mutual recursion does not). `INSPECT($var)` lowers to a dedicated AST node carrying the variable unevaluated (`MODULE::INSPECT` is rejected). `PATH_TYPE` is AST-only by design. `EXPORT` is reserved for future script-module support.
- [breaking] Host Rust API only, scripts are unaffected (#146): `Engine` is generic over the process manager (`Engine<P: ProcessManager = DefaultProcessManager>`) and covers the former power-path-only surface: `with_io` stages custom IO, `register_module` stages host libraries, `run_script_on` / `run_steps_on` run on caller-built filesystems with any manager, and every run returns the final cwd, filesystem handle, and top-level variable bindings. `run_script` / `run_steps` keep their default-plumbing shapes but return the run output instead of `()`. `run_script` / `run_script_on` parse with the engine's module table (`STD` plus staged modules). First-party hosts (docs-gen) run through the facade.
- [breaking] Host Rust API only, scripts are unaffected (#146): `Value` and `ValuePayload` fields are private, so safe code can no longer forge words with struct literals. Read the descriptor via `Value::descriptor()` / `Value::type_name()` and peek at payload bits via `Value::inline_bits()` / `Value::heap_ptr()`; construction still flows through `Value::mint_heap` / `mint_inline` and the typed constructors.
- [breaking] Host Rust API only, scripts are unaffected (#146): the value-word model carries vtables instead of integer ids, so `TypeId`, the `TYPE_*` constants, the descriptor table functions, `make_custom_value` / `make_inline_value`, and the `ExecState::register_type` id return are gone; mint through `Value::mint_heap` / `mint_inline` with `T::descriptor()`, read back with `read_heap` / `read_inline` against a descriptor, and pass `&'static TypeDescriptor` to `register_type` / `run_steps_with_manager_with_modules`. `TYPES()` and `TYPE_DESCRIBE()` read the run's name directory on the AST path. The legacy untyped `host_funcs` map and `HostFn` are deleted: every callable registers through a `HostModule` into the single registry. `register_fn` / `register_host(s)` are gone with no alias; `parse_script_with_hosts` is `parse_script_with_modules`.
- [breaking] Scripts are affected: unknown modules, unknown functions, unimported bare calls, and ambiguous bare names (two imported modules exporting one name) fail at parse time with span-accurate errors instead of runtime `unknown function` failures. Runtime step errors name the base callable (`INT() expects ...`, `FUNC BOOM`) while listings and `DESCRIBE` stay qualified. Registering a duplicate qualified name panics, including host collisions with `STD` builtins.
- [breaking] Host Rust API only, scripts are unaffected (#152): `TypeDescriptor` has a new `unshare` field, so host code constructing descriptors by literal (instead of `#[oxdock_type]`) must add it; macro-derived types recompile unchanged.

### Fixed

- `ArgType::Int` lower-time validation is 64-bit: literals like `EXIT 3000000000` lower instead of failing while `INT` itself always accepted them.
- Build fingerprinting sees every environment read: `collect_env_references` now walks `env:KEY` in `LET`/`SET`/`CALL`/`RETURN` expressions, `IF`/`WHILE` conditions, `FOR` iterables, `INHERIT_ENV` keys, and mixed `Arg::Parts` fragments, instead of only command template fields and guards. Previously-missed reads could leave stale cached assets in place.
- Command reference corrections: `INHERIT_ENV [<key>, ...]` syntax, `ASSERT_EQ --hash` taking no positional expected value, repeated `ELSE IF`, optional `FUNC` params and bare `RETURN`, case-insensitive `WORKSPACE` target, `INT(42)` type-name casing, `UpperCamelCase` registration markers, accurate `--hash`/`TIMEOUT` prose, and no-hoisting semantics in `LET`. Failing reference examples now print their `**Expected error:**` needle instead of hiding it in the fence.

### Dependencies

- Bump `toml_edit` 0.25.13+spec-1.1.0 → 0.25.15+spec-1.1.0 (#150).
- Bump `pest_derive` / `pest_generator` 2.9.0 → 2.9.1 (#149).
- Bump `toml` 1.1.5+spec-1.1.0 → 1.1.6+spec-1.1.0 (#148).

### Removed

- [breaking] `TypeKind` / `Value` enums and the descriptor table: type references in scripts are plain names resolved against the run's name directory at coercion time (unknown names fail there, not at parse); values are constructed via `Value::int/float/string/...` and read via `as_*` accessors. `TypeKind::Custom` / `Value::Custom` enum variants are gone.
- [breaking] `NativeRegistry` is now `FunctionRegistry`, unifying DSL `FUNC` definitions (scoped, shadowing restores on scope exit) with native and host entries. `RUN` exec-form elements reject task handles with a type error instead of stringifying them.

## [0.15.0-alpha] - 2026-09-16

### Added

- Typed parse errors (#143): `oxdock-parser` now returns `ParseError` / `ParseErrorKind` (`PestParse`, `InvalidSyntax`, `UnknownCommand`, `Structural`, `Validation`) instead of untyped strings, with a 1-based line number, column span, source line, caret block, expected syntax, and hint on every string path error. Sub-expression spans are refined to the offending token (for example the `BOOL` in a bad `FOR` key type), and multi-line bodies resolve source lines from the shared script text.
- Error handling documentation with executing examples (#143): the workspace README gains a "When a script fails to parse" section whose `rust` doctests run the real parser and pin the exact output, plus a parser README section describing the error model. Exact outputs are also pinned in `error_kinds.rs` integration tests.

### Fixed

- Syntax errors no longer surface as `unknown command` (#143): any line starting with a known command or structural keyword (`WITH_IO`, `LET`, `FOR`, `IF`, and the rest) always fails as `invalid syntax for command X` with the expected syntax and a concrete example; only truly unknown names report `unknown command`, with a `did you mean` hint when only the case is wrong. Malformed `WITH_IO` bindings explain the binding rules, and lowercase commands keep the uppercase correction note with a caret.
- CLI renders parse errors with `Display` (`{err:#}`) instead of `Debug`, preserving the multi-line caret block.

### Changed

- [breaking] Host Rust API only, scripts are unaffected: `oxdock-parser` entrypoints (`parse_script`, `lower_command`, and the structural lowerers) return `Result<T, ParseError>` instead of `anyhow::Result<T>`. Downstream crates convert into their own error types at the outer boundary; message text is unchanged so substring assertions keep passing.

## [0.14.1-alpha] - 2026-09-15

### Fixed

- Lazy snapshot materialized by assertion-needle pre-registration (#131): `resolve_arg_state` built a full command context just to read environment bindings, and constructing the context resolved the working directory through the snapshot choke point, creating the `oxdock-XXXX` temp directory before the first step ran. Needle expansion now reads `state.envs` directly without touching the filesystem, so `ECHO` / `ASSERT_CONTAINS`-only scripts leave the snapshot pending under both `WORKSPACE LOCAL` and the default snapshot root, and their failures report `never materialized` instead of dumping a marker-only tree.
- Bare statement keywords as arg values broke `Display` round-trips (#144): `WRITE a IF` rendered the `IF` contents unquoted because the quoting guard only recognized `Command` registry entries, while the token-stream walker splits lines on structural keywords too, so re-parsing failed with `invalid syntax for command IF`. Quoting now consults the shared `STRUCTURAL_KEYWORDS` registry alongside `Command::parse`, and the token walker uses the same predicate, so every registered keyword round-trips as a value in both parse pathways. A grammar-conformance test parses `dsl.pest` with `pest_meta` (new `oxdock-parser` dev-dependency) and locks the registry against the grammar in both directions, following rule references transitively from the top-level instruction rules instead of relying on line formatting or rule-name conventions.

## [0.14.0-alpha] - 2026-09-14

### Added

- Lazily-created snapshot workspace (#131): the snapshot temp directory is no longer created up front. Scripts that only use `WORKSPACE LOCAL` (or run empty) never create a snapshot directory at all; everything else materializes it exactly once, on first snapshot use. `WORKSPACE SNAPSHOT` alone only selects without creating, and `RUN` under `WORKSPACE LOCAL` executes against the live tree without materializing.
- `CARGO_TARGET_DIR` isolation (#131): `RUN` steps now point `CARGO_TARGET_DIR` at a reserved scratch location instead of `<snapshot>/.cargo-target`, so nested `cargo` invocations can no longer write into the snapshot workdir or the live workspace tree. The scratch name is reserved but never created by the host; `cargo` creates it on demand.
- Host-side variable bindings export (#140): `LazyRunOutput` now carries `bindings: BTreeMap<String, Value>` with the top-level script variables captured at completion (function, loop, and background scopes are excluded; nothing is exported on failure). `ExecutionResult` forwards the same map, and `run_steps_with_manager` is public for hosts that run against their own filesystem handle and need the bindings alongside the final cwd.

### Changed

- Test suite audit from file-backed to memory-resident assertions (#140): pipe-drain `WRITE` steps in `ast_commands` fixtures now assert exact `[pipes.*] expect` values or `expect.stdout`, and pure value-expression integration tests assert run bindings or captured output instead of writing files and re-reading them. No DSL or engine behavior changed.
- [breaking] Host Rust API only, scripts are unaffected: `ExecutionResult::tempdir: GuardedTempDir` is now `ExecutionResult::snapshot: Arc<LazyGuardedTempDir>` with `has_snapshot()` / `snapshot_path()` helpers, and `final_cwd` points under the workspace root when no snapshot was created. `oxdock-build` / `oxdock-macros` emit an empty output dir for `WORKSPACE LOCAL`-only scripts instead of syncing from a snapshot.
- [breaking] Renamed the guard predicate `neq(...)` to `ne(...)` (#140) for a single spelling consistent with the `Ne` comparison family; the old spelling no longer parses.

### Removed

- [breaking] `ASSERT_FILE`, `ASSERT_STDOUT`, `ASSERT_DIR`, and `ASSERT_ABSENT` are removed in favor of `ASSERT_EQ`, `ASSERT_CONTAINS`, and the `PATH_TYPE("path")` query expression. Assertions now operate purely on evaluated values with no implicit I/O: `ASSERT_EQ $status 200` compares typed values with no coercion, `ASSERT_CONTAINS stdout "error"` checks stream buffers, and file content enters through explicit reads (`LET $content: STRING = READ "config.txt"` plus `ASSERT_EQ $content ...`). Metadata checks become queries (`ASSERT_EQ PATH_TYPE("src") "dir"`, `ASSERT_EQ PATH_TYPE("tmp") "absent"`). Bare `stdout` / `stderr` / `pipe:NAME` in first-argument position observe stream and pipe buffers; quoting stays interchangeable everywhere. `EntryKind` gains a `Symlink` variant reported only by the new no-follow inspection. See the command reference for the full syntax matrix.

## [0.13.0-alpha] - 2026-09-11

### Added

- DSL arithmetic with full numeric support (#112): `LET $x: INT = 2 + 3 * 4` binds `14`, with `*`/`/` binding tighter than `+`/`-`, unary minus (`-5`, `2 * -3`), and parentheses nesting arbitrarily (`2 * (2 * (2 + 3)) * 4`). `42` is an `INT` literal and `3.14` a `FLOAT` literal; bare words that merely start with digits keep their literal reading (`30s`, `100ms`, `123/456`, `1.0.0`, `-f` stay strings).
- Numeric semantics (#112): `Int x Int` stays `INT` (checked math, truncating integer division, so `7 / 2` is `3`); any `Float` operand promotes the result to `FLOAT` (`1 + 2.5` is `3.5`). Division by zero, overflow, and non-finite results are runtime errors rather than stored values.
- Ordering comparisons (#112): `< <= > >=` alongside the existing `== !=`, with numeric semantics when both sides are numbers (`1 == 1.0` is true). `==`/`!=` on anything else keep comparing rendered strings, and ordering non-numerics is a Type Error. Chained comparisons are a parse error (`$a < $b < $c` is rejected); write the conjunction explicitly (`$a < $b && $b < $c`).
- `INT()` / `FLOAT()` conversions (#112): the explicit bridge from captured command output (which is always a string) to numbers, so `LET $total: INT = $total + INT($size_str)` accumulates. `INT` trims ASCII whitespace and rejects non-integers; `FLOAT` accepts int strings and rejects non-finite input. Plain string operands never convert implicitly: `"100" + 1` is a Type Error.
- Float equality documented with runnable examples (#112): equality is exact with no epsilon, so binary fractions compare cleanly (`0.5 + 0.25 == 0.75` is true) while decimal fractions may not (`0.1 + 0.2 == 0.3` is false, the sum is `0.30000000000000004`). The reference explains why (power-of-2 denominators) and shows bounding instead (`IF $sum > 0.299999 && $sum < 0.300001`).
- Logical operators documented with examples: `&&` binds tighter than `||`, both short-circuit (`IF true || $missing` never touches the right side), and only `Bool` conditions are accepted.
- Bash comparison table in the `LET` reference: capture looks like `output=$(...)` but keeps exact bytes (Bash strips all trailing newlines), stays explicitly typed, converts only via `INT()`/`FLOAT()`, and fails the step immediately when the captured command fails.
- Scope semantics documented and pinned: mutating an outer variable inside a block persists after exit for every type (`LET $x` outside, `$x = ...` inside), while `LET` inside a block declares a shadow that reverts. Binding and mutation convert to the declared type (`$n = "42"` binds `42` for an `INT`).

### Fixed

- Spaced `&&` / `||` chains failed to parse (`a && b && c`): whitespace is now accepted around every chained operator, not just the first. (The flaw predates arithmetic; the new arithmetic tiers ship with the corrected shape, so `100 / 10 / 2` chains too).
- Reference pages no longer leak internal identifiers (`coerce_value`, `ExecState`, `declare_var`); user docs say "convert to the declared type".

### Changed

- Bare `1/0`-style words now parse as arithmetic: previously `LET $x: STRING = 1/0` bound the string `"1/0"` because no `/` operator existed; now that `/` is division, `LET $x: INT = 1/0` is a division-by-zero error. Quoted strings are unaffected.

## [0.12.0-alpha] - 2026-09-11

### Added

- Variables now declare their type up front (#130): `LET $count: INT = 0`, with types `STRING, INT, FLOAT, BOOL, PIPE, LIST, MAP, HANDLE, DURATION, PATH` (`INT` is 64-bit, `FLOAT` is 64-bit). Declaring the same name twice in one scope is an error; change it later with bare `$count = 2`.
- Reading environment variables is explicit (#130): `LET $e: STRING = env:FOO` reads `FOO` into a plain string, while a bare `$var` never touches the environment (templates still use `{{ env:KEY }}`). There is no `ENV` type.
- Loop variables carry types too (#130): `FOR $item: STRING IN ...`, with `INT` or `STRING` keys (`INT` gives the 0-based list index; maps need `STRING` keys).
- User-defined functions (#114): `FUNC GREET($name: STRING) { ... }` defines a reusable block (names are UPPERCASE, parameters carry types like `LET`). Run it with `CALL GREET("ada")`, or capture what it returns with `LET $r: STRING = CALL GREET("ada")`. A function without `RETURN` gives back an empty string, and anything it prints still shows up normally.
- `WHILE` loops (#114): `WHILE !$done { ... }` repeats while the condition holds (must be true/false, like `IF`). Each round gets a fresh scope, so change an outer variable (`$done = true`) to exit.
- `BREAK` and `CONTINUE` (#114): work in both `FOR` and `WHILE`, always affecting the innermost loop. Using them outside a loop, or across a function or background-task boundary, is an error.
- Background function calls (#114): `LET $t: HANDLE = ASYNC CALL WORK("job")` runs a function in the background; `LET $o: STRING = AWAIT $t` waits and gives back its return value. Calls nest at most 64 deep, and going deeper fails with an error naming the function.
- Rust embedders (#114): host-side functions can be registered under the same UPPERCASE `CALL` names that script functions use; user-facing help output for them comes later.
- Explicit pipe handles (#114): `pipe:NAME` names a pipe without touching a stream (`LET $p: PIPE = pipe:log`), mirroring `env:KEY`. A fresh name registers on first use, so pipes can be declared before any `WITH_IO` mentions them.
- Variable pipe bindings (#114): `WITH_IO [stdout=$p]` / `[stdin=$p]` resolve a PIPE-typed variable against the live pipe registry when the step runs; undeclared, mistyped, or missing names are step-numbered errors. Pipes created, bound, or passed by variable inside functions are always script pipes: OS promotion never crosses a `CALL` boundary.
- `INSPECT($var)` (#114): snapshots a variable into a MAP with its declared type plus live details — pipe backend stats (`is_os_pipe`, `buffer_bytes`, `readers`, `writers`), task phase for handles — so scripts and fixtures can assert engine state directly.

### Fixed

- `$var = ...` reassignment inside `{ ... }` blocks (loop and function bodies) was silently ignored, so loop counters and `WHILE` exit flags never updated. Only top-level reassignment used to work.

### Changed

- [breaking] `LET` requires an explicit type, so `LET $x = ...` is now a parse error; reassignment is bare `$x = ...` and there is no `SET` keyword (a `SET ...` line fails with a hint); `FOR` variables require type tags; there is no `ENV` type; bare `$var` never reads the environment (use `env:KEY` or `{{ env:KEY }}`); command-reference Type cells show real types only (`$var` / `KEY=value` shapes display as the `STRING` they bind or resolve to).
- [breaking] plain strings no longer become pipes: `LET $p: PIPE = "log"` is now a TypeMismatch error even when a pipe of that name exists; use `pipe:log`.

### Dependencies

- Bump `cargo_metadata` 0.19.2 → 0.23.1.
- Bump `pest` 2.9.0 → 2.9.1.
- Bump `syn` 3.0.4 → 3.0.5.
- Bump `toml` 1.1.4+spec-1.1.0 → 1.1.5+spec-1.1.0.

## [0.11.0-alpha] - 2026-09-09

### Added

- Unified `LET` output capture: `LET $x = <sync command>` runs the command to completion and binds its exact stdout bytes into `$x` (no newline stripping; commands with no stdout bind `""`; non-UTF8 stdout is an error), spilling to a guarded temp file past 8 MiB instead of buffering unboundedly in memory.
- `LET $o = AWAIT $t` captures a background task's stdout into `$o`; bare `AWAIT $t` keeps its status semantics and now forwards the task's stdout to the parent stdout.
- Pipe backlog and capture share one spillable sink backed by `GuardedPath::tempdir` (PID-lock GC) instead of `std::env::temp_dir`, with the same 8 MiB spill / 100 MiB backlog-cap behavior; spills stay memory-only under Miri.
- `RUN ["exe", "arg", ...]` exec form: spawns the executable directly with no shell, so there is no shell expansion, globbing, redirection, or pipes; use it for portable commands. Elements accept quoted strings, bare words, `$var` / `$a.b`, and `CALL()`; quoted `{{ ... }}` templates interpolate per element while `\$` / `\{{` escapes pass through literally, and `;` / `//` inside elements stay literal. Guards and wrappers (`ASYNC`, `TIMEOUT`, `WITH_IO`) apply to both forms; `RUN []` is an error and shell `RUN <command...>` behavior is unchanged.
- `ProcessManager::run_argv` / `spawn_argv` for direct executable spawning across the `Shell`, `Mock`, and Miri `Synthetic` backends, plus documented `INHERIT_STDOUT_ENV_VAR` / `PROCESS_DEBUG_ENV_VAR` constants replacing hardcoded environment variable names.
- `WITH_IO` wrapping an `ASYNC` block whose body is a single `RUN`, guarded or not, now promotes the pipe to a zero copy OS kernel pipe: the producer child writes straight into the kernel and a concurrent `RUN` consumer reads straight out, with no copies through memory buffers. DSL consumers (`WRITE`, `READ`, ...) on a live name keep working through a bridged reader. All other shapes keep the in memory script pipe, so sequential fan in, keepers, DSL bodies, and host injected pipes behave exactly as before. Promotion is single producer single consumer by construction: a second producer or consumer on a live name fails deterministically instead of interleaving bytes. The consumer must run while the producer is alive, since output past the 64 KiB kernel buffer stalls until drained. Under Miri everything stays on script pipes with identical results for small payloads.

### Changed

- `LET $x = WITH_IO [stdin=pipe:p] <sync command>` now captures instead of failing; combining capture with an explicit `WITH_IO [stdout=pipe:...]` is a parse error since the capture sink owns stdout.
- Named `ASYNC` tasks no longer share the parent stdout writer: output is buffered per task and surfaces via `AWAIT` (forward), `LET $o = AWAIT $t` (bind), or end-of-pipeline reaping for tasks that are never awaited.
- Host Rust API only, scripts are unaffected: `CommandOptions.stdin` is now a `CommandStdin` enum instead of `Option<SharedInput>`. Rust embedders replace `stdin: Some(x)` with `stdin: CommandStdin::Stream(x)` and `stdin: None` with `stdin: CommandStdin::Null`. `CommandStdout` and `CommandStderr` gain matching host only `OsPipe` variants for direct kernel pipe handoff.

### Fixed

- `WITH_IO` docs describe both pipe modes: `ASYNC` single `RUN` pipelines use zero copy OS kernel pipes, sequential steps use script pipes (memory plus 8 MiB spill). The old "named pipes" and "without temp files" wording is removed.

## [0.10.0-alpha] - 2026-09-08

### Added

- Command reference examples demonstrating variable scoping: `ENV` and `LET` assignments inside a braced block revert when the block exits, and `EXPAND` `KEY=val` overrides shadow the environment for that call only.
- `WORKDIR` description now documents relative-to-current-directory resolution, `/` reset to the workspace root, and the sandbox guarantee.
- `ASSERT_*` descriptions now document abort-on-mismatch failure semantics, plus a new `ASSERT_FILE --hash` example.
- Declared argument types are mechanically enforced: static literals type-check at lower time and variables/templates validate on their resolved values at runtime. `SLEEP`/`TIMEOUT` accept dynamic durations (`SLEEP $d`, `TIMEOUT $d`) instead of freezing literals at parse.
- `COPY --from-current-workspace` reference example.
- docs-gen exits non-zero when any target fails to render, with a regression test pinning the behavior.

### Changed

- [docs-gen] READMEs are now assembled from a master template per document: the section order you see in the template file is the order you get in the README. Sections shared between documents live in one file and are pulled in by name, and a misspelled section name fails the build instead of silently rendering wrong docs. Two doc-only renames came along with it: every fragment now ends in `.md.tmpl`, and the `oxdock-build` fragment with parens in its name became `usage-build-rs.md`.

### Fixed

- `EXIT` with a non-integer code is now an error instead of silently exiting 0.
- Trailing positionals beyond a command's declared arity fail lowering instead of being silently discarded; tail-joining commands (`ECHO`, `WRITE`/`APPEND` contents, `ASSERT_FILE` expected text, `ASSERT_STDOUT`, `EXPAND` overrides, `INHERIT_ENV` keys) declare variadic `Rest` args so legitimate multi-word use keeps working.
- Command reference argument/flag tables escape `|` in type strings (e.g. `SNAPSHOT|LOCAL`), which previously split the WORKSPACE row into extra columns on strict Markdown renderers.
- `SLEEP` summary reworded from "Sleep without spawning a shell" to "Pause execution for a duration".

## [0.9.0-alpha] - 2026-09-07

### Added

- `docs-gen` rebuilt as a general-purpose doc engine: ordered `template` / `read` / `glob` / `text` stages executed as pure OxDock DSL (`$var` bindings only, no hand-built AST), config-driven targets discovered from each crate's `.oxdock/template` directory, and plugin data providers (`command-ref`, `cargo-metadata`) with per-target value overrides.
- Sparse `target.json` files (just `name`/`out`) synthesize stages from the target directory layout (`header.tmpl`, verbatim `fragments/*.md`, expanded `fragments/*.tmpl`, `footer.tmpl`); bespoke targets declare full stages.
- Generated command reference shared three ways from one provider: root README, `oxdock` README, and rustdoc includes (`oxdock/docs/command_reference.md`, `crates/oxdock-parser/docs/command_reference.md`).
- Shared embed example consumed by both the root and `oxdock` READMEs from a single canonical file.
- Master-template targets: order comes from `{{> path }}` positions in an `output.tmpl` document instead of a managed JSON stage list (verbatim unless `.tmpl`, which expands); literal document prose expands with the values context.

### Fixed

- Quoted values with spaces parse identically in every command: `ENV SET_FORTH="outer scope"` stores `outer scope` instead of truncating, and `EXPAND tmpl KEY="a b"` no longer fails with "accepts at most one path".
- `KEY=$var` env values and `EXPAND` overrides evaluate the variable (parity with `ECHO $var`); `KEY="{{ $var }} tail"` interpolates with the literal tail kept.
- Multi-assignment lines split uniformly (`EXPAND K1=$x K2=$y` yields two overrides); `ENV` with more than one assignment is a precise error instead of silently merging or dropping values.
- `$var` mixed into `ECHO` / `RUN` / `WRITE` tails is preserved instead of silently dropped (`ECHO $x hello` keeps the value).
- Both `"` and `'` quotes strip in `ENV` values (previously `"` only), and values split on the first `=` (`KEY=a=b` stores `a=b`).
- `GLOB()` patterns containing `..` match nothing instead of traversing outside the sandbox root; every glob result is validated against the workspace boundary.
- `GLOB()` lists sandbox contents on Windows (verbatim `\\?\`-prefixed roots no longer yield empty results).
- Windows PID liveness probe treats `ERROR_ACCESS_DENIED` as alive (parity with Unix `EPERM`), so tempdir cleanup never reaps another live process's directories.

### Changed

- `ENV` / `EXPAND` reference docs rewritten: what a template is, placeholder namespaces and precedence, override value rules, and runnable proof examples for every value form.
- `FOR` / `LET` / `ECHO` reference docs enriched (`GLOB("*")` quoting rule and end-to-end example, expression-only `LET` right-hand side, variable `ECHO` forms, piped-stdin `EXPAND`).

## [0.8.0-alpha] - 2026-09-05

### Added

- `EXPAND` command for template expansion of a file or stdin to stdout, with `KEY=val` overrides alongside `{{ env:KEY }}` interpolation.
- `ASYNC` / `AWAIT` / `CANCEL` for background tasks, including block form and `LET $t = ASYNC { ... }` handles.
- `TIMEOUT <duration> <command|block>` and `SLEEP <duration>` for deadline control and delays.
- `READ_LINE $var` for line-oriented reads into a variable.
- `FOR` (value and key-value forms), `IF` / `ELSE IF` / `ELSE`, and `LET` / `ASSIGN` with expression support (`==` / `!=`, `!` negation, `&&` / `||`, `GLOB(...)`).
- Guard expressions: `!` / `not(...)`, `any(...)` / `all(...)`, `eq(...)` / `neq(...)`, `bool:<val>`; `[guard]` prefixes on `LET` / `ENV` / `WORKDIR` / `WORKSPACE` blocks.
- Unified block scoping for braced blocks (`IF`, `FOR`, `TIMEOUT`, `ASYNC`, `WITH_IO`) with scope unwind on nested `EXIT`.
- `oxdock` facade crate as the canonical entry point; bare `cargo run` launches the CLI.
- CLI `--help` / `-h` usage output, positional script paths, and `-` / `--script -` stdin handling.
- `oxdock!` proc-macro for inline DSL with `#var` host interpolation, including `FOR` / `LET` blocks and `GLOB(#var)`.
- `WorkspaceFs::open_read` / `open_write` / `open_append` streaming file I/O across Host, Miri, and Mock backends.

### Changed

- Data pipeline handlers (`WRITE` / `APPEND`, `EXPAND`, `HASH_SHA256`, `ASSERT_STDOUT`) stream in fixed-size chunks instead of buffering whole inputs.
- `ASSERT_STDOUT` uses bounded per-step matching instead of an unbounded stdout log; windows are re-expanded on `ENV` / `INHERIT_ENV` mutations.
- `RUN_BG` semantics superseded by `ASYNC` task handles with `AWAIT` / `CANCEL`; `RAW_WRITE` superseded by `{{ env:KEY }}` interpolation.
- Per-arg quoting tracked via `Arg::String` / `Arg::Expr` (quoted `--flags` stay positional).
- `Guard` inversion replaced by composable `not()` / `!` and `eq` / `neq` / `bool` guards.
- Single-site command registry generating step kinds, lowering, and metadata; unknown-command errors include structural and casing hints.
- Crate renames: `oxdock-buildtime-helpers` to `oxdock-build`, `oxdock-buildtime-macros` to `oxdock-macros`; `embed!` / `prepare!` to `oxdock_embed!` / `oxdock_prepare!`.
- Docs generated from the command registry; `pulldown-cmark` replaces the custom markdown parser.
- `spawn_interactive_shell` moved to `oxdock-process`; CLI runner is a thin delegate.
- Test layout collapsed to single integration binaries per crate with a `slow-integration` feature gate.
- `oxdock-fs` path handling normalized for Windows CI parity.
- Bump `syn` 2.0.119 → 3.0.4.

### Removed

- `RUN_BG` command (use `ASYNC`); `RAW_WRITE` command (use interpolation).
- Old `oxdock-buildtime-helpers` / `oxdock-buildtime-macros` crate names and bare `embed!` / `prepare!` macro names.
- Unbounded `ExecState::stdout_log`; legacy `expand_with_lookup`; custom markdown parser; `DocSpec` in `docs-gen`; orphaned `oxdock-process` `test_utils` module.

### Fixed

- Template `}}` split across chunk boundaries is detected; empty input preserves pending expansion state.
- `open_append` on the Miri backend appends instead of overwriting from position 0.
- `ASSERT_STDOUT` falls through to step-scope matching only on empty stdin; error output includes buffered content for debugging.
- Long-needle assertions no longer truncate history prematurely.
- Unknown-command diagnostics suggest structural statements and correct casing.
- Nested `EXIT` unwinds `LET` / `ENV` / `WORKDIR` / `WORKSPACE` scopes without leaking.
- `CANCEL` on completed tasks and double-`CANCEL` succeed without affecting unrelated tasks; `TIMEOUT` kills only the timed-out task.

## [0.7.0-alpha] - 2026-08-27

### Refactoring

- **oxdock-core**: Split `exec.rs` into focused modules: `handlers`, `fs_ops`, `io`, `pipe`, `state`, `steps`, and `tests` for better maintainability.
- **oxdock-process**: Decomposed `lib.rs` into `builder`, `shell`, `child`, `contract`, `expand`, `shell_manager`, `synthetic`, and `builtin_env` modules.

### Added

- `APPEND` command for cross-platform append-only file writes (ideal for GitHub Actions `$GITHUB_OUTPUT`, `$GITHUB_ENV`, `$GITHUB_STEP_SUMMARY`).
- GitHub Actions Integration section in README documenting `ECHO`, `RUN`, and `APPEND` patterns for workflow commands.
- Markdown DSL parsing support (`oxdock-parser/src/markdown.rs`).
- `OXDOCK_EMBED_FINGERPRINT_SALT` environment variable for cache busting.
- `ASSERT_STDOUT` and `ASSERT_ABSENT` command prototypes.
- Docs conformance tests and packaging invariant tests.
- Expanded README with comprehensive documentation.

### Fixed

- Fuzz parity test failure: filter out strings that fail `proc_macro2` lexing instead of panicking.

### Dependencies

- Bump `anyhow` 1.0.100 → 1.0.104.
- Bump `libc` 0.2.178 → 0.2.189.
- Bump `libtest-mimic` 0.8.1 → 0.8.2.
- Bump `line-ending` 1.5 → 1.5.1.
- Bump `pest`/`pest_derive` 2.8.4 → 2.9.0.
- Bump `proc-macro2` 1.0.103 → 1.0.107.
- Bump `proptest` 1.9.0 → 1.11.0.
- Bump `quote` 1.0.42 → 1.0.47.
- Bump `sha2` 0.10.9 → 0.11.0 (with API migration).
- Bump `syn` 2.0 → 2.0.119.
- Bump `tempfile` 3.24.0 → 3.27.0.
- Bump `toml_edit` 0.24.0 → 0.25.13.
- Update all transitive dependencies via `cargo update`.
