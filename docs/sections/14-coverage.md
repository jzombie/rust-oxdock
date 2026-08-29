## Testing & Coverage

### Testing

Testing is performed across Linux, Mac, and Windows environments, and UB (Undefined Behavior) testing is handled by [Miri](https://github.com/rust-lang/miri).

There is strong prioritization in keeping unit and integration tests compatible with Miri, because doing so also encourages clean separation of process and filesystem modeling from direct OS calls, avoiding scattered filesystem and process usage throughout the codebase.

### Coverage reporting

#### LLVM line coverage (cargo-llvm-cov)

The `coverage (cargo-llvm-cov)` GitHub Actions job installs [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) and publishes [`lcov`](https://github.com/linux-test-project/lcov) data to Coveralls. Once the repository is enabled on Coveralls, pushes and pull requests to `main` automatically update the badge above.

To reproduce the report locally (requires the nightly LLVM tools component):

```bash
cargo install cargo-llvm-cov
rustup component add llvm-tools-preview
cargo llvm-cov --workspace --all-features --lcov --output-path lcov.info
```

#### Miri coverage

The CI `miri` job monitors how many workspace unit tests can run under [`cargo miri`](https://github.com/rust-lang/miri). On pushes to `main`, the job publishes a badge description (`badges/miri-coverage.json` on the `badges` branch) that backs the Miri coverage badge above.

To keep the badge grounded in real coverage reporting, the workflow multiplies two signals:

1. **Runnable test ratio:** how many workspace tests are runnable under Miri vs. the total (`cargo miri test -- --list`).
2. **LLVM line coverage baseline:** the percent reported by `cargo llvm-cov --summary-only` (the same value sent to Coveralls).

The badge therefore shows an approximate “effective Miri coverage” (baseline coverage × runnable ratio), which can never exceed the standard coverage percentage but gives a tangible sense of how much of the tested surface area is validated under the runner.

To test the calculation locally without waiting for CI:

```bash
cargo llvm-cov --workspace --all-features --summary-only > coverage-summary.txt
BASE_LINE_COVERAGE=$(awk '/^TOTAL/ {print $10}' coverage-summary.txt | tr -d '%' | head -n1) \
  scripts/.github/miri-badge-report.sh
```

The helper emits the same badge JSON (`badges/miri-coverage.json`) and summary text used by CI, making it easy to confirm the numbers before opening a PR.

If you run new tests under Miri locally, you can sanity-check parity with CI via:

```bash
cargo +nightly miri setup
cargo +nightly miri test --workspace --all-features --lib --tests
```
