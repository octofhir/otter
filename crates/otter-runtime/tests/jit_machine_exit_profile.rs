//! Optimized exits feed the next generation's speculation.
//!
//! # Contents
//! - A hot function whose int32 multiply overflows on most calls, so the
//!   int32-only operand feedback keeps describing a site whose result leaves
//!   the int32 range.
//!
//! # Invariants
//! - The exit PCs recorded by the first optimized generation widen the baked
//!   feedback of those instructions, so the rebuilt generation lowers the site
//!   as Float64 arithmetic and stops exiting; the function keeps its
//!   optimizing generation instead of being abandoned.
//! - Every tier returns the interpreter's completion.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
// Both operands are always int32; only the product leaves the int32 range,
// which operand feedback cannot observe. The loop keeps `grow` out of its
// caller's inline tree so the exit is attributed to `grow` itself.
function grow(seed) {
  let v = 0;
  for (let k = 0; k < 2; k++) v = (seed + k) * 100003;
  return v;
}
function drive(rounds) {
  let acc = 0;
  for (let i = 0; i < rounds; i++) acc += grow(i + 30000);
  return acc;
}
drive(30000);
"#;

fn run(selection: JitSelection) -> (String, otter_runtime::RuntimeExecutionStats) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-machine-exit-profile.js",
        )
        .expect("overflowing multiply")
        .completion_string()
        .to_owned();
    (completion, runtime.execution_stats())
}

#[test]
fn an_overflowing_int32_site_exits_once_per_budget_and_then_stays_float() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, stats) = run(JitSelection::ProductionTiered);
    assert_eq!(compiled, oracle);
    assert!(
        stats.jit_optimized_entries + stats.jit_generated_optimizing_entries > 1000,
        "the function must keep an optimizing generation: {stats:?}"
    );
    // The first generation may exit up to its reoptimization budget at the
    // overflowing site; the rebuilt generation must not exit there again, and
    // no third generation is ever needed.
    assert!(
        stats.jit_optimized_deopts <= 100,
        "the rebuilt generation must not repeat the exited speculation: {stats:?}"
    );
}
