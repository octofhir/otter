//! One widening recompile after an Int32 overflow or negative-zero exit.
//!
//! # Contents
//! - `Neg` producing `-0`, `Increment` / `AddImm` / `Add` leaving Int32, each
//!   first exiting from an optimizing whole-function entry.
//! - The same exits from a callee spliced into its optimized caller.
//! - The unmodified `arith-exit-repair` benchmark kernel.
//!
//! # Invariants
//! - Every optimizing exit kind (entry, OSR, inlined deopt, generated
//!   linkage) widens the exiting site's arithmetic feedback exactly once and
//!   retires the generation; later calls run the widened code without exits.
//!   A result the interpreter or baseline code already saw leave Int32 never
//!   exits at all.
//! - Results match the interpreter oracle exactly.

use otter_runtime::{JitSelection, Runtime, RuntimeExecutionStats, SourceInput};
use otter_vm::native_abi::{CodeLifetimeState, NativeFrameKind};

/// The completion, the run's statistics, and how many functions' entries
/// select an installed optimizing generation once it finishes.
fn run(source: &str, selection: JitSelection) -> (String, RuntimeExecutionStats, usize) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("arithmetic repair runtime");
    let completion = runtime
        .run_script(SourceInput::from_javascript(source), "arith-exit-repair.js")
        .unwrap_or_else(|error| panic!("arithmetic repair fixture: {error:?}"))
        .completion_string()
        .to_owned();
    let optimized = runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .filter(|generation| {
            generation.tier == NativeFrameKind::Optimizing
                && generation.lifecycle == CodeLifetimeState::Installed
                && generation.current_entry
        })
        .count();
    (completion, runtime.execution_stats(), optimized)
}

fn assert_repaired_once(source: &str, expected: &str, optimized_functions: usize) {
    let (oracle, _, _) = run(source, JitSelection::InterpreterOnly);
    assert_eq!(oracle, expected);
    let (compiled, stats, optimized) = run(source, JitSelection::ProductionTiered);
    assert_eq!(compiled, oracle);
    assert!(
        optimized >= optimized_functions,
        "the fixture's functions must stay optimized: {optimized} current, {stats:?}"
    );
    assert!(
        stats.jit_optimized_deopts <= 4,
        "each exiting site must widen once, not exit per call: {stats:?}"
    );
}

#[test]
fn entry_exits_widen_their_site_once() {
    const ENTRY: &str = r#"
function negate(count) {
    let total = 0;
    for (let index = 0; index < count; index++) total += Math.abs(-(index & 3) * 0.5);
    return total;
}
function step(start, count) {
    let value = start;
    for (let index = 0; index < count; index++) value = value + 1;
    return value;
}
function accumulate(start, count) {
    let total = start;
    for (let index = 0; index < count; index++) total = total + (index & 7);
    return total;
}
let checksum = 0;
for (let call = 0; call < 400; call++) {
    checksum += negate(64);
    checksum += step(2147483600, 64) - 2147483600;
    checksum += accumulate(2147483600, 64) - 2147483600;
}
String(checksum);
"#;
    assert_repaired_once(ENTRY, "134400", 3);
}

#[test]
fn inlined_callee_exits_widen_the_innermost_site() {
    const INLINED: &str = r#"
function flip(value) { return -value; }
function bump(value) { return value + 1; }
function caller(count) {
    let total = 0;
    for (let index = 0; index < count; index++) {
        total += Math.abs(flip(index & 3) * 0.5) + (bump(2147483647 - (index & 1)) - 2147483647);
    }
    return total;
}
let checksum = 0;
for (let call = 0; call < 400; call++) checksum += caller(64);
String(checksum);
"#;
    assert_repaired_once(INLINED, "32000", 1);
}

#[test]
fn arith_exit_repair_kernel_stays_optimized() {
    const KERNEL: &str = include_str!("../../../benchmarks/scripts/arith-exit-repair.js");
    let source = format!(
        "{KERNEL}\nlet last = 0;\nfor (let call = 0; call < 20; call++) last = engineKernel();\nString(last);"
    );
    assert_repaired_once(&source, "150000", 1);
}
