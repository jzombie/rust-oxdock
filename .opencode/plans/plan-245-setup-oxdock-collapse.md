# Plan: #245 Collapse setup-oxdock into installer stubs

## Goal

Remove PowerShell (and Bash) from `.github/actions/setup-oxdock/action.yml`,
collapsing it into the installer stubs it downloads. End state: the action
is one shell line running the native stub with (version, dir, name).
Every workflow already funnels through `setup-oxdock`, so it is the single
bootstrap point. No per-workflow changes.

Decision (issue comment): keep native stub per OS (`install.ps1` on
Windows, `install.sh` elsewhere). Git-Bash-everywhere was rejected
(self-hosted Windows may lack it; both stubs stay exercised every run).

## Current state

`action.yml` runs three `pwsh` steps around the installer:

1. `release`: resolve `latest` via GitHub API, download `install.sh` /
   `install.ps1`, run with `INSTALL_DIR=RUNNER_TEMP/oxdock-engine`, write
   `OXDOCK_ENGINE_DIR` / `OXDOCK_EXPECTED_VERSION` to `GITHUB_ENV`.
2. `local` only: `cargo build`, copy binary, `--version` check, write
   `GITHUB_ENV`.
3. Always: rename `oxdock` to `oxdock-engine` (Windows file lock: the
   engine cannot rename the file it runs from), then invoke fresh engine
   on `post-install.oxfile` (smoke + `GITHUB_PATH`).

`install.sh:50` and `install.ps1:43-45` already resolve `latest`
themselves. `install.oxfile` already takes `INSTALL_DIR` via `OXDOCK_DIR`,
verifies hash, installs to `$dir/oxdock[.exe]`.

## Target shape

Push each piece into the stub:

- Version resolution: delete action API call, pass `VERSION` through.
  Stub resolves `latest` itself.
- Final name: accept name/dir in stub, extract under it directly.
  Kills rename step and Windows-lock workaround.
- Smoke + `GITHUB_PATH`: stub execs fresh engine on CI tail before
  exiting (bundling holds: remote consumers check out only action dir).
- `local`: `cargo build` stays in shell (cargo's job), then reuses same
  place + smoke tail.

## Constraint: CI extras stay opt-in

`install.sh` / `install.ps1` are also the public installers
(`curl | bash` behaves exactly as today), so CI behavior gates on env
presence:

- `oxdock-engine` naming (users want bare name; CI frees it).
- `GITHUB_ENV` / `GITHUB_PATH` writes (meaningless off-runner; gate on
  files existing).
- Hard `--version`-vs-expected gate.

Shared logic (resolve, download, extract, smoke-run) stays common to
both paths. Smoke matrix must cover installer defaults unchanged.

## Design: single installer for users and CI

There is one install path. `install.sh` / `install.ps1` + `install.oxfile`
serve `curl | bash` users and CI identically. CI only adds opt-in env;
when absent, behavior is byte-identical to today. This is what makes the
installer testable: every CI run exercises the public resolve, download,
verify, extract code, plus a gated tail. A side-channel `post-install`
invoked only from `action.yml` would isolate the installer from CI and
leave the documented user path unproven, so it is rejected.

### New env contract (stubs -> oxfile)

- `VERSION` pass-through from action input (stub resolves `latest`).
- `INSTALL_DIR` already exists, action sets to `RUNNER_TEMP/oxdock-engine`.
- `OXDOCK_BINARY_NAME` (new, default `oxdock`): final stem; oxfile
  appends `.exe` on Windows if missing.
- `OXDOCK_EXPECTED_VERSION` (derived): stub computes `${VERSION#v}` /
  `TrimStart('v')`; DSL has no string surgery so stub does it.
- `OXDOCK_SKIP_DOWNLOAD` (new, `local` only): when `1`, bypass hash +
  `INSTALL` and run only the CI tail against the pre-placed binary.
  Absent/false preserves today's user path byte-identical.
- `GITHUB_PATH`, `GITHUB_ENV`: only honored inside the nested CI guard.

CI tail fires only inside a single nested guard: outer checks
`$GITHUB_PATH` names an existing file first, inner checks
`OXDOCK_EXPECTED_VERSION` non-empty. No independent sequential `IF`s, so
a lone leaked var can never trigger a partial tail. Off-runner the outer
check fails, so nothing CI-specific executes.

### install.sh changes

- Accept `OXDOCK_BINARY_NAME`, export to oxfile env.
- After resolution, derive `EXPECTED`, export as
  `OXDOCK_EXPECTED_VERSION`.
- After install.oxfile succeeds, run fresh `$DIR/$NAME` on CI tail when
  gated env present. Keep warm-cache, digest, `OXDOCK_RELEASE_BASE`,
  `OXDOCK_OXFILE_URL`, `OXDOCK_ENGINE`, `OXDOCK_CACHE_DIR` identical.

### install.ps1 changes

- Mirror of `install.sh`: accept `OXDOCK_BINARY_NAME`, derive expected
  version, run fresh binary on CI tail gated on `GITHUB_PATH` /
  expected-version presence. Keep `Test-CachedAsset`, stub
  verification, piped-stdin bridge identical.

### install.oxfile changes (fold tail in, single path)

- `INHERIT_ENV += [OXDOCK_BINARY_NAME, OXDOCK_EXPECTED_VERSION, GITHUB_PATH, GITHUB_ENV, OXDOCK_SKIP_DOWNLOAD]`.
- `LET $dest = "{{ $dir }}/{{ env:OXDOCK_BINARY_NAME || oxdock }}"`,
  `[family:windows]` appends `.exe` when missing.
- `IF "{{ env:OXDOCK_SKIP_DOWNLOAD }}" != "1"` then today's hash +
  `INSTALL` payload; else skip straight to the tail (assumes `$dest`
  already placed by `cargo build` + copy). No download happens inside
  the oxfile either way; the stub owns fetch, the flag only skips
  verify + place.
- After place/skip, single nested CI guard (never two root-level `IF`s):
  outer `PATH_TYPE("{{ env:GITHUB_PATH }}") == "file"`, inner
  `"{{ env:OXDOCK_EXPECTED_VERSION }}" != ""`, then `--version` assert
  + `APPEND` dir + `GITHUB_ENV` appends together or not at all.
- Keeps one fetch, no new release asset / `SHA256SUMS` entry / cache key.
- No separate CI-only script: `post-install.oxfile` is deleted and its
  conformance pins move to `install.oxfile`. One installer, exercised by
  CI on every run, documented once.

### action.yml collapse

- `release`: one step, native stub one-liner, e.g.
  `VERSION=${{ inputs.version }} INSTALL_DIR=$RUNNER_TEMP/oxdock-engine
  OXDOCK_BINARY_NAME=${{ inputs.binary-name }} bash install.sh`
  (pwsh equivalent on Windows). No API call, no `GITHUB_ENV` math, no rename.
- `local`: `cargo build --release -p oxdock`, copy directly as
  `$binary-name` into `RUNNER_TEMP/oxdock-engine`, then run the built
  engine on `install.oxfile` with `OXDOCK_SKIP_DOWNLOAD=1`,
  `OXDOCK_DIR`, `OXDOCK_BINARY_NAME`, `OXDOCK_EXPECTED_VERSION`
  (from `--version`), `GITHUB_PATH` set. Same tail, no download, no
  shell-duplicated asserts.
- Delete rename + `OXDOCK_ENGINE_BIN` bridge.
- Audit `OXDOCK_ENGINE_DIR` readers: workflows only need `PATH` after
  publish; confirm nothing else reads it before removing export.

### post-install.oxfile fate

- Delete. Its smoke + publish logic lives in `install.oxfile` behind the
  dual gate above. Update `setup_action_conformance.rs` to pin the
  folded contract instead.

### Release + tests

- `rust-release.yml`: unchanged (no new asset).
- `install-smoke.yml`: must still prove defaults (bare `oxdock`,
  default dirs, `curl|bash` / `irm|iex`, cache reuse/heal, offline,
  newest). No leg depends on CI env.
- `setup_action_conformance.rs`: update `INHERIT_ENV`, `APPEND`,
  `ASSERT_EQ` markers to folded contract.
- `smoke_script_conformance.rs`: `binary-name: oxdock-engine` markers
  stay; arch matrix unchanged.

## Prototype without touching main

- `OXDOCK_RELEASE_BASE` serves branch tarballs locally.
- `OXDOCK_OXFILE_URL` tests modified `install.oxfile` unreleased.
- `OXDOCK_ENGINE` runs install logic under branch-built engine.

Loop: branch build -> tarballs as artifacts -> `workflow_dispatch`
prototype running stub with new flags across `install-smoke.yml` OS
matrix. Land when green.

## Out of scope

Readiness retries / exit-code branching blocked on #243. `if:` /
`matrix` / `uses:` YAML stays.

## Sequencing

1. Fold `OXDOCK_BINARY_NAME` + `OXDOCK_SKIP_DOWNLOAD` + nested dual
   gate into `install.oxfile` (defaults absent = today's behavior).
2. Teach both stubs new env + fresh-engine tail; prototype via
   overrides on loopback.
3. Collapse `action.yml` to one-liner (+ `local` tail).
4. Update conformance tests, land via full smoke matrix.
