//! A runtime built without execution statistics leaves them off its results.
//!
//! # Invariants
//!
//! - Statistics are on by default: a run's result carries them.
//! - Turned off, a result carries none, and the runtime still answers when
//!   asked for them directly.

use otter_runtime::{Runtime, SourceInput};

const WORK: &str = "let o = { a: 1 }; let s = 0; for (let i = 0; i < 100; i++) s += o.a; s";

#[test]
fn results_carry_statistics_by_default() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let result = runtime
        .run_script(SourceInput::from_javascript(WORK), "<work>")
        .expect("a run");
    assert_eq!(result.completion_string(), "100");
    assert!(result.stats().work_units_executed > 0);
}

#[test]
fn a_runtime_without_statistics_leaves_them_off_its_results() {
    let mut runtime = Runtime::builder()
        .execution_stats(false)
        .build()
        .expect("runtime");
    let result = runtime
        .run_script(SourceInput::from_javascript(WORK), "<work>")
        .expect("a run");
    assert_eq!(result.completion_string(), "100");
    let stats = result.stats();
    assert_eq!(
        (
            stats.work_units_executed,
            stats.bytecode_calls,
            stats.property_load_hits
        ),
        (0, 0, 0)
    );
    assert!(runtime.execution_stats().work_units_executed > 0);
}
