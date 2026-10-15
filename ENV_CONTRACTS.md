# Environment Variable Contracts

Every variable the engine seeds into script environments.

> Rendered by `cargo run -p docs-gen` from the contract declaration in
> `oxdock_process::env_policy`. Do not edit by hand: the declaration is
> the single source, and the table below renders it verbatim.

Script authors route non-essential host values explicitly with
`INHERIT_ENV`. Everything else below arrives mirrored from the host,
without declaration.

## Seeded environment

Session start copies host values for the declared names directly
into the script environment map. A host lacking a name leaves it
absent: nothing is synthesized, nothing is immutable. Explicit
script `ENV`/`INHERIT_ENV` overwrite entries uniformly; `ISOLATED`
resets the map to the same seed. A missing value surfaces at spawn
time, with the exact `INHERIT_ENV` declaration to add.

### Host-mirrored defaults

Declared names mirrored from the host at session start and on every
`ISOLATED` entry, wherever the host defines them. Absent stays
absent; explicit script values win.

| Variable | Platforms |
| --- | --- |
| [`PATH`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+PATH&type=code) | unix, windows |
| [`HOME`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+HOME&type=code) | unix |
| [`TMPDIR`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+TMPDIR&type=code) | unix |
| [`TERM`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+TERM&type=code) | unix |
| [`LOGNAME`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+LOGNAME&type=code) | unix |
| [`USER`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+USER&type=code) | unix |
| [`TEMP`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+TEMP&type=code) | windows |
| [`TMP`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+TMP&type=code) | windows |
| [`USERPROFILE`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+USERPROFILE&type=code) | windows |
| [`LOCALAPPDATA`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+LOCALAPPDATA&type=code) | windows |
| [`APPDATA`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+APPDATA&type=code) | windows |
| [`SystemRoot`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+SystemRoot&type=code) | windows |
| [`windir`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+windir&type=code) | windows |
| [`PATHEXT`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+PATHEXT&type=code) | windows |
| [`ProgramFiles`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+ProgramFiles&type=code) | windows |
| [`ProgramFiles(x86)`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+ProgramFiles%28x86%29&type=code) | windows |
| [`ProgramW6432`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+ProgramW6432&type=code) | windows |
| [`COMSPEC`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+COMSPEC&type=code) | windows |
| [`SystemDrive`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+SystemDrive&type=code) | windows |
| [`PROCESSOR_ARCHITECTURE`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+PROCESSOR_ARCHITECTURE&type=code) | windows |
| [`CARGO_TARGET_DIR`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+CARGO_TARGET_DIR&type=code) | unix, windows |

### Explicit routing

The defaults in the table above mirror the host. Everything else
enters scripts only when listed in `INHERIT_ENV` (inside `ISOLATED`,
only when listed in the block allowlist, which copies from the
enclosing scope). Absent stays absent.

### Host reads

Variables the engine process itself touches.

| Variable | Read by |
| --- | --- |
| [`SHELL`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+SHELL&type=code) | oxdock_process::shell::shell_program |
| [`COMSPEC`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+COMSPEC&type=code) | oxdock_process::shell::shell_program |
| [`TMPDIR`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+TMPDIR&type=code) | LazyGuardedTempDir::new (via std::env::temp_dir) |
| [`CARGO_FEATURE_*`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+CARGO_FEATURE_%2A&type=code) | BuiltinEnv::collect |
| [`CARGO_CFG_*`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+CARGO_CFG_%2A&type=code) | BuiltinEnv::collect |

### Precedence and enforcement

Uniform script-wins: the seed populates mirrors, and explicit
script `ENV`/`INHERIT_ENV` overwrite them like any other variable.
No name is immutable. `ISOLATED` resets to the identical seed, so
inner and outer scopes start from the same defaults.
`check_spawn_anchors` validates the final staged map with no
caller-specific flags: present anchors pass, absent ones bail with
the exact `INHERIT_ENV` declaration to add.

Source of truth: the `define_env_contract!` invocation in
`crates/sys/oxdock-process/src/env_policy.rs` (names live in
`crates/sys/oxdock-fs/src/env.rs`). This file is generated; do not
edit by hand.

### Engine plumbing

Names the engine and Cargo provide to the host process itself.
Read at explicit call sites; reaches scripts only through the
sanctioned seed or explicit routing. Nothing flows ambiently.

| Variable | Owner |
| --- | --- |
| [`OXDOCK_CACHE_DIR`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_CACHE_DIR&type=code) | oxdock |
| [`OXDOCK_CACHE_APP`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_CACHE_APP&type=code) | oxdock |
| [`oxdock`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+oxdock&type=code) | oxdock |
| [`OXDOCK_WORKSPACE_ROOT`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_WORKSPACE_ROOT&type=code) | oxdock |
| [`OXDOCK_EMBED_DEBUG`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_EMBED_DEBUG&type=code) | oxdock |
| [`OXDOCK_EMBED_FINGERPRINT_SALT`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_EMBED_FINGERPRINT_SALT&type=code) | oxdock |
| [`OXDOCK_EMBED_FORCE_REBUILD`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_EMBED_FORCE_REBUILD&type=code) | oxdock |
| [`OXDOCK_BANNER`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_BANNER&type=code) | oxdock |
| [`OXDOCK_INHERIT_STDOUT`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_INHERIT_STDOUT&type=code) | oxdock |
| [`OXDOCK_SHELL_REEXEC`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_SHELL_REEXEC&type=code) | oxdock |
| [`OXDOCK_FIXTURE_TARGET_DIR`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_FIXTURE_TARGET_DIR&type=code) | oxdock |
| [`OXDOCK_AST_CASE`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_AST_CASE&type=code) | oxdock |
| [`OXDOCK_AST_ONLY_COVERAGE`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_AST_ONLY_COVERAGE&type=code) | oxdock |
| [`OXDOCK_MULTI_GUARD_TEST_PASS`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_MULTI_GUARD_TEST_PASS&type=code) | oxdock |
| [`OXDOCK_MULTI_GUARD_TEST_FAIL`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_MULTI_GUARD_TEST_FAIL&type=code) | oxdock |
| [`OXDOCK_GUARD_TEST_TOKEN_UNSET`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OXDOCK_GUARD_TEST_TOKEN_UNSET&type=code) | oxdock |
| [`CARGO_MANIFEST_DIR`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+CARGO_MANIFEST_DIR&type=code) | cargo |
| [`CARGO_PKG_NAME`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+CARGO_PKG_NAME&type=code) | cargo |
| [`CARGO_PRIMARY_PACKAGE`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+CARGO_PRIMARY_PACKAGE&type=code) | cargo |
| [`CARGO_TARGET_DIR`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+CARGO_TARGET_DIR&type=code) | cargo |
| [`CARGO_INCREMENTAL`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+CARGO_INCREMENTAL&type=code) | cargo |
| [`CARGO_CFG_FEATURE`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+CARGO_CFG_FEATURE&type=code) | cargo |
| [`OUT_DIR`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+OUT_DIR&type=code) | cargo::build |
| [`TARGET`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+TARGET&type=code) | cargo::build |
| [`PROFILE`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+PROFILE&type=code) | cargo::build |
| [`RUSTC`](https://github.com/search?q=repo%3Ajzombie%2Frust-oxdock+RUSTC&type=code) | cargo::build |
