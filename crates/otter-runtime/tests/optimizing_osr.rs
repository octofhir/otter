//! Optimizing on-stack replacement and post-entry deoptimization parity.
//!
//! # Contents
//! - A once-called float array read-modify-write loop that can only reach the
//!   optimizing tier through a hot back-edge.
//! - An int32 loop that overflows after optimized OSR and resumes from its
//!   reconstructed interpreter frame.
//! - A once-called loop reading one invariant property, whose access the
//!   optimizing tier moves into a pre-header the OSR entry never reaches.
//!
//! # Invariants
//! - Tiered and interpreter-only runs execute identical source.
//! - The tiered run records an optimizing OSR entry, not merely template OSR.
//! - A post-OSR deopt preserves all loop-carried values needed for completion.
//! - A loop whose invariant access was hoisted still has an OSR entry, which
//!   performs that access itself before entering the loop.

use otter_runtime::{JitSelection, Runtime, RuntimeExecutionStats, SourceInput};

const ARRAY_RMW: &str = r#"
    const osrA = [];
    const osrB = [];
    for (let setup = 0; setup < 2048; setup = setup + 1) {
      osrA[setup] = 1.25;
      osrB[setup] = 0.5;
    }

    function onceFloatRmw(a, b, scale, limit) {
      let total = 0.5;
      for (let index = 0; index < limit; index = index + 1) {
        a[index] = a[index] * scale + b[index];
        total = total + a[index];
      }
      return total;
    }

    const osrRmwResult = onceFloatRmw(osrA, osrB, 0.5, 2048);
    JSON.stringify({ result: osrRmwResult, first: osrA[0], last: osrA[2047] });
"#;

const POST_OSR_DEOPT: &str = r#"
    function onceOverflow(limit) {
      let index = 0;
      let total = 2147483000;
      while (index < limit) {
        total = total + 100;
        index = index + 1;
      }
      return total;
    }
    String(onceOverflow(20));
"#;

const HOISTED_INVARIANT_READ: &str = r#"
    function OsrHolder(value) { this.slot = value; }
    const osrHolder = new OsrHolder(7);

    function onceInvariantRead(holder, limit) {
      let total = 0;
      for (let index = 0; index < limit; index = index + 1) {
        total = total + holder.slot;
      }
      return total;
    }
    String(onceInvariantRead(osrHolder, 4096));
"#;

fn run(source: &str, selection: JitSelection, url: &str) -> (String, RuntimeExecutionStats) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(4)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(SourceInput::from_javascript(source), url)
        .expect("OSR fixture")
        .completion_string()
        .to_owned();
    (completion, runtime.execution_stats())
}

#[test]
fn once_called_float_array_loop_enters_optimized_osr() {
    let (oracle, _) = run(
        ARRAY_RMW,
        JitSelection::InterpreterOnly,
        "optimizing-osr-rmw-oracle.js",
    );
    let (tiered, stats) = run(
        ARRAY_RMW,
        JitSelection::ProductionTiered,
        "optimizing-osr-rmw-tiered.js",
    );

    assert_eq!(tiered, oracle);
    assert_eq!(oracle, r#"{"result":2304.5,"first":1.125,"last":1.125}"#);
    assert!(
        stats.jit_optimized_osr_entries >= 1,
        "once-called loop must enter the optimizing tier through OSR: {stats:?}"
    );
}

#[test]
fn osr_entry_performs_the_pre_header_access_a_hoisted_loop_reads() {
    let (oracle, _) = run(
        HOISTED_INVARIANT_READ,
        JitSelection::InterpreterOnly,
        "optimizing-osr-hoist-oracle.js",
    );
    let (tiered, stats) = run(
        HOISTED_INVARIANT_READ,
        JitSelection::ProductionTiered,
        "optimizing-osr-hoist-tiered.js",
    );

    assert_eq!(tiered, oracle);
    assert_eq!(oracle, "28672");
    assert!(
        stats.jit_optimized_osr_entries >= 1,
        "a hoisted loop keeps its OSR entry: {stats:?}"
    );
}

#[test]
fn deopt_after_optimized_osr_reconstructs_loop_state() {
    let (oracle, _) = run(
        POST_OSR_DEOPT,
        JitSelection::InterpreterOnly,
        "optimizing-osr-deopt-oracle.js",
    );
    let (tiered, stats) = run(
        POST_OSR_DEOPT,
        JitSelection::ProductionTiered,
        "optimizing-osr-deopt-tiered.js",
    );

    assert_eq!(tiered, oracle);
    assert_eq!(oracle, "2147485000");
    assert!(stats.jit_optimized_osr_entries >= 1, "{stats:?}");
    assert!(
        stats.jit_optimized_deopts >= 1,
        "overflow after OSR must reconstruct and resume the frame: {stats:?}"
    );
}
