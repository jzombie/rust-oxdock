## Selective environment inheritance

Scripts no longer inherit the caller's environment wholesale. Host variables stay private unless you opt in explicitly.

- Add `INHERIT_ENV [FOO, BAR, BAZ]` at the very top of the script to copy those keys from the process environment before any other command runs.
- The directive must be top-level—no guards, no surrounding blocks, and no repeats. Trying to nest or guard it triggers a parser error so scripts stay deterministic.
- Subsequent `ENV` commands can override inherited values, similar to how Docker's `ENV` overrides `--env` flags.
- Test harnesses and embedders can supply values programmatically; the [environment-guards example](#environment-guards) injects `DEPLOY_TARGET` through the docs-conformance runner rather than the real process environment.

Keeping inheritance selective avoids leaking secrets by default while still allowing ergonomics for well-known keys (proxy settings, artifact caches, etc.).
