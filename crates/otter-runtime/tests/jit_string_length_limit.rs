//! Exact string-length overflow across interpreter and generated concat paths.
//!
//! # Contents
//! - Thirty-one cheap rope doublings that reach length `2^31` without
//!   materialising the string.
//! - The thirty-second doubling, which must exact-deopt and raise one catchable
//!   `RangeError` rather than saturating the rope length or reporting OOM.
//! - A second script on the same runtime proving normal concatenation remains
//!   usable after the caught boundary error.
//!
//! # Invariants
//! - `JsString` length is an exact `u32`; no rope body carries a saturated sum.
//! - The allocating JIT stub classifies logical overflow as a pre-effect miss,
//!   while genuine allocation refusal remains its distinct OOM outcome.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SETUP: &str = r#"
function doubleStringAtLimit(value) {
  return value + value;
}

for (let warm = 0; warm < 5000; warm++) {
  doubleStringAtLimit("warm");
}
"#;

const PROBE: &str = r#"
let boundary = "a";
for (let power = 0; power < 31; power++) {
  boundary = doubleStringAtLimit(boundary);
}

let catches = 0;
let isRangeError = false;
let errorName = "";
let errorMessage = "";
try {
  doubleStringAtLimit(boundary);
} catch (error) {
  catches++;
  isRangeError = error instanceof RangeError;
  errorName = error.name;
  errorMessage = error.message;
}

JSON.stringify([
  boundary.length,
  catches,
  isRangeError,
  errorName,
  errorMessage
]);
"#;

const REUSE: &str = r#"
JSON.stringify([doubleStringAtLimit("reuse")]);
"#;

#[derive(Debug)]
struct Run {
    probe: String,
    reuse: String,
    optimized_entries: u64,
    optimized_deopts: u64,
    stub_ok: u64,
    stub_miss: u64,
    stub_oom: u64,
}

fn run(selection: JitSelection) -> Run {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("string length runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(SETUP),
            "jit-string-length-limit-setup.js",
        )
        .expect("warm generated string concat");

    let before = runtime.execution_stats();
    let probe = runtime
        .run_script(
            SourceInput::from_javascript(PROBE),
            "jit-string-length-limit-probe.js",
        )
        .expect("catch the exact string length error")
        .completion_string()
        .to_owned();
    let after = runtime.execution_stats();

    let reuse = runtime
        .run_script(
            SourceInput::from_javascript(REUSE),
            "jit-string-length-limit-reuse.js",
        )
        .expect("runtime remains usable after caught string length error")
        .completion_string()
        .to_owned();

    Run {
        probe,
        reuse,
        optimized_entries: after.jit_optimized_entries - before.jit_optimized_entries,
        optimized_deopts: after.jit_optimized_deopts - before.jit_optimized_deopts,
        stub_ok: after.jit_alloc_value_stub_ok - before.jit_alloc_value_stub_ok,
        stub_miss: after.jit_alloc_value_stub_miss - before.jit_alloc_value_stub_miss,
        stub_oom: after.jit_alloc_value_stub_out_of_memory
            - before.jit_alloc_value_stub_out_of_memory,
    }
}

#[test]
fn thirty_second_doubling_throws_once_and_runtime_remains_usable() {
    let oracle = run(JitSelection::InterpreterOnly);
    let compiled = run(JitSelection::ProductionTiered);

    assert_eq!(compiled.probe, oracle.probe);
    assert_eq!(
        compiled.probe,
        r#"[2147483648,1,true,"RangeError","Invalid string length"]"#
    );
    assert_eq!(compiled.reuse, oracle.reuse);
    assert_eq!(compiled.reuse, r#"["reusereuse"]"#);

    assert!(
        compiled.optimized_entries >= 32,
        "all 31 valid doublings and the overflow probe must enter generated code: {compiled:?}"
    );
    assert_eq!(
        compiled.optimized_deopts, 1,
        "only the pre-effect overflowing concat may deopt: {compiled:?}"
    );
    assert!(
        compiled.stub_ok >= 31,
        "every legal doubling must complete through the allocating stub: {compiled:?}"
    );
    assert_eq!(
        compiled.stub_miss, 1,
        "string-too-long must be one exact stub miss: {compiled:?}"
    );
    assert_eq!(
        compiled.stub_oom, 0,
        "logical string overflow must never be reported as heap OOM: {compiled:?}"
    );
}
