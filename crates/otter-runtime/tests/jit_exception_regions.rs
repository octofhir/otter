//! Production template-tier structured-exception completion coverage.
//!
//! # Contents
//! - Catch/finally entry and normal handler removal.
//! - Throw and abrupt return completion through nested finally bodies.
//! - Break/continue crossing finally, including replacement of parked state.
//! - A direct compiled callee throw caught by its compiled caller.
//!
//! # Invariants
//! - Loop OSR is disabled for the fixture, so exception-bearing functions must
//!   compile and enter as whole functions.
//! - Observable counters prove committed throws/finally bodies are not replayed
//!   after a machine-code continuation exit.
//! - Template completion is byte-for-byte identical to the interpreter oracle.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
let state = {
  catchEffects: 0,
  finallyEffects: 0,
  returnEffects: 0,
  jumpEffects: 0,
  calleeEffects: 0,
  callerCatches: 0
};
function caughtAndFinalized(x, counters) {
  let result = 0;
  try {
    counters.catchEffects++;
    if (x % 3 === 0) throw { value: x, marker: "caught" };
    result = x;
  } catch (error) {
    result = error.value + 1000;
  } finally {
    counters.finallyEffects++;
    result += 10;
  }
  return result;
}

function returnThroughFinally(x, counters) {
  try {
    if ((x & 1) === 0) return x + 1;
    return x;
  } finally {
    counters.returnEffects += x;
  }
}

function jumpThroughFinally(limit, counters) {
  let score = 0;
  outer: for (let i = 0; i < limit; i++) {
    try {
      if (i === 2) continue outer;
      if (i === 5) break outer;
      score += i;
    } finally {
      counters.jumpEffects++;
      score += 10;
      if (i === 3) continue outer;
    }
  }
  return score;
}

function throwingCallee(x, counters) {
  counters.calleeEffects++;
  if (x % 7 === 0) throw { value: x, self: null };
  return x;
}

function compiledCaller(x, counters) {
  try {
    return throwingCallee(x, counters);
  } catch (error) {
    counters.callerCatches++;
    error.self = error;
    return error.self.value + 2000;
  }
}

// Warm both direct-call endpoints before the measured caller sequence. These
// functions have no hot inner loop: with the OSR threshold held at u32::MAX,
// execution can become native only through whole-function entry compilation.
for (let i = 1; i < 80; i++) throwingCallee(i * 7 + 1, state);
for (let i = 1; i < 80; i++) compiledCaller(i * 7 + 1, state);

let caughtSum = 0;
let returnSum = 0;
let jumpSum = 0;
let callerSum = 0;
for (let i = 0; i < 192; i++) {
  caughtSum += caughtAndFinalized(i, state);
  returnSum += returnThroughFinally(i, state);
  jumpSum += jumpThroughFinally(8, state);
  callerSum += compiledCaller(i, state);
}

JSON.stringify([
  caughtSum, state.catchEffects, state.finallyEffects,
  returnSum, state.returnEffects,
  jumpSum, state.jumpEffects,
  callerSum, state.calleeEffects, state.callerCatches
]);
"#;

fn run(selection: JitSelection) -> (String, u64, u64, u64, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-exception-regions.js",
        )
        .expect("exception-region matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (
        completion,
        stats.jit_compile_attempts,
        stats.jit_osr_attempts,
        stats.jit_reentrant_stub_transitions,
        stats.jit_generated_calls,
    )
}

#[test]
fn exception_regions_complete_from_whole_function_entries() {
    let (oracle, _, _, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, entry_attempts, osr_attempts, reentrant, direct_calls) =
        run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(entry_attempts > 0, "fixture must compile function entries");
    assert_eq!(osr_attempts, 0, "fixture must not tier through loop OSR");
    assert!(
        reentrant > 0,
        "exception opcodes must execute the shared reentrant transition"
    );
    assert!(
        direct_calls > 0,
        "throw propagation must cross a compiled direct-call boundary"
    );
}
