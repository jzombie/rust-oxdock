//! Interpreter throughput tripwires.
//!
//! Two pure-DSL loops run in-process and assert exact results, so
//! correctness is pinned alongside speed. The wall-clock bounds are
//! deliberately generous (30x release timings, headroom for dev
//! profiles and loaded CI runners): they catch orders-of-magnitude
//! regressions like accidental quadratic scope handling, not 10%
//! slips. For real numbers, run with `-- --nocapture`: each test
//! prints its iterations and elapsed time.

use anyhow::{Context, Result};
use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::GuardedPath;
use std::time::{Duration, Instant};

fn run_script(script: &str) -> Result<()> {
    let steps = oxdock_core::parse_script(script).expect("perf script parses");
    let workspace_temp = GuardedPath::tempdir().context("workspace tempdir")?;
    let context_temp = GuardedPath::tempdir().context("context tempdir")?;
    let fs_root = workspace_temp.as_guarded_path().clone();
    let context_root = context_temp.as_guarded_path().clone();
    run_steps_with_context_result_with_io(&fs_root, &context_root, &steps, ExecIo::new())
        .map(|_| ())
}

/// 100k arithmetic iterations. A release binary runs this end to
/// end in ~0.3s; in-process dev takes ~1s.
#[test]
#[cfg_attr(miri, ignore = "wall-clock bounds are meaningless under Miri")]
fn arithmetic_loop_throughput() {
    let start = Instant::now();
    run_script(indoc! {r#"
        LET $n: INT = 0
        LET $i: INT = 0
        WHILE $i < 100000 {
          $n = $n + $i
          $i = $i + 1
        }
        ASSERT_EQ $n 4999950000
    "#})
    .expect("arithmetic loop completes with the exact sum");
    let elapsed = start.elapsed();
    eprintln!("arithmetic_loop: 100000 iters in {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(60),
        "arithmetic loop took {elapsed:?}, expected under 60s"
    );
}

/// 20k iterations through a function call with interpolation and trim.
/// A release binary runs this end to end in ~0.2s.
#[test]
#[cfg_attr(miri, ignore = "wall-clock bounds are meaningless under Miri")]
fn call_and_string_loop_throughput() {
    let start = Instant::now();
    run_script(indoc! {r#"
        IMPORT [STD]
        FUNC WORK($x: INT) {
          RETURN TEXT_TRIM("  {{ $x }}  ")
        }
        LET $i: INT = 0
        LET $last: STRING = ""
        WHILE $i < 20000 {
          $last = WORK($i)
          $i = $i + 1
        }
        ASSERT_EQ $last "19999"
    "#})
    .expect("call loop completes with the exact final value");
    let elapsed = start.elapsed();
    eprintln!("call_and_string_loop: 20000 iters in {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(60),
        "call loop took {elapsed:?}, expected under 60s"
    );
}
