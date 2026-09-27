#!/usr/bin/env bash
# This helper is invoked from GitHub Actions only; it inspects Miri test
# runnable ratio and writes badge artifacts for the README. Do not run
# manually in production workflows.
#
# The badge reports exactly what is measured: how many workspace tests are
# runnable under Miri vs. the total (`-- --list` vs. `-- --ignored --list`).
set -euo pipefail

MIRI_TEST_CMD=${MIRI_TEST_CMD:-"cargo miri test --workspace --all-features --lib --tests"}

run_listing() {
  bash -c "$MIRI_TEST_CMD $1" | grep -c ': test' || true
}

total=$(run_listing "-- --list --format terse")
ignored=$(run_listing "-- --ignored --list --format terse")
run_cnt=$((total - ignored))

runnable_ratio_value="0.0"
ratio_label="n/a"
if [ "$total" -gt 0 ]; then
  runnable_ratio_value=$(awk -v r="$run_cnt" -v t="$total" 'BEGIN { printf "%.1f", (r / t) * 100 }')
  ratio_label="${runnable_ratio_value}%"
fi

ratio_int=$(printf "%.0f" "$runnable_ratio_value")
if [ "$ratio_int" -ge 90 ]; then
  color="brightgreen"
elif [ "$ratio_int" -ge 80 ]; then
  color="green"
elif [ "$ratio_int" -ge 70 ]; then
  color="yellow"
else
  color="red"
fi

message="${ratio_label} (${run_cnt}/${total})"
mkdir -p badges
{
  printf "Miri runnable tests: %s/%s\n" "$run_cnt" "$total"
  printf "Runnable ratio: %s\n" "$ratio_label"
} | tee miri-summary.txt
printf "runnable_ratio=%s\n" "${ratio_label}" | tee miri-output.env
printf '{"schemaVersion":1,"label":"miri coverage","message":"%s","color":"%s"}\n' "$message" "$color" | tee badges/miri-coverage.json

if [ -n "${GITHUB_OUTPUT:-}" ]; then
  cat miri-output.env >> "$GITHUB_OUTPUT"
fi

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### Miri coverage"
    echo
    cat miri-summary.txt
  } >> "$GITHUB_STEP_SUMMARY"
else
  cat miri-summary.txt
fi
