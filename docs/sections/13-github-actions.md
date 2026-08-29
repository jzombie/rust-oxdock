## GitHub Actions Integration

OxDock scripts can emit GitHub Actions workflow commands using native DSL primitives.
All examples below use `[env:GITHUB_ACTIONS]` guards so they execute on CI runners
but are skipped during local `docs_conformance` tests.

### Log annotations

`ECHO` writes to stdout, which GitHub Actions intercepts for annotations:

```oxdock
ECHO "::notice::test notice message"
ECHO "::warning::test warning message"
ECHO "::error::test error message"
```

### Collapsible log groups

```oxdock
RUN echo "::group::unit tests"
RUN echo "running tests"
RUN echo "::endgroup::"
```

### Job summary, step outputs, and environment variables

`APPEND` writes to append-only runner state files without truncating earlier entries:

```oxdock
APPEND dist/summary.md "### Build Report\n- Passed: 123\n- Failed: 0\n"
APPEND dist/outputs.txt "artifact_path=dist/app.tar\n"
APPEND dist/env.txt "NOTEBOOK_MODE=release\n"
```

On GitHub Actions, replace the paths with the runner-provided env vars (`{{ env:GITHUB_STEP_SUMMARY }}`, `{{ env:GITHUB_OUTPUT }}`, `{{ env:GITHUB_ENV }}`):
