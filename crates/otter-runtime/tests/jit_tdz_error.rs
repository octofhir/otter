//! Production template-tier TDZ throw materialization coverage.
//!
//! # Contents
//! - A compiled callee executing `TdzError` after an observable getter.
//! - A compiled caller catching the propagated value as a `ReferenceError`.
//! - Interpreter/template output and side-effect parity.
//!
//! # Invariants
//! - The TDZ VM error is materialized as the interpreter's catchable error
//!   object before the compiled throw epilogue runs.
//! - Caller resumption never replays the getter or loses thrown-value identity.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_exception_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
let getterEffects = 0;
const state = {
  effects: 0,
  catches: 0,
  get tick() {
    getterEffects++;
    return 1;
  }
};

function tdzCallee(trigger, counters) {
  counters.effects += counters.tick;
  if (trigger) return local;
  let local = 1;
  return local;
}

function tdzCaller(trigger, counters) {
  try {
    return tdzCallee(trigger, counters);
  } catch (error) {
    counters.catches++;
    return error instanceof ReferenceError && error.name === "ReferenceError";
  }
}

for (let i = 0; i < 80; i++) tdzCallee(false, state);
for (let i = 0; i < 80; i++) tdzCaller(false, state);

let caught = 0;
for (let i = 0; i < 120; i++) {
  if (tdzCaller(true, state)) caught++;
}

JSON.stringify([caught, state.catches, state.effects, getterEffects]);
"#;

fn run(selection: JitSelection) -> (String, u64, u64, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-tdz-error.js",
        )
        .expect("tdz matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (
        completion,
        stats.jit_compile_attempts,
        stats.jit_reentrant_stub_transitions,
        stats.jit_generated_calls,
    )
}

#[test]
fn compiled_caller_catches_materialized_tdz_reference_error() {
    let (oracle, _, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, entry_attempts, reentrant, direct_calls) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert_eq!(compiled, "[120,120,280,280]");
    assert!(entry_attempts > 0, "fixture must compile function entries");
    assert!(
        reentrant > 0,
        "TdzError must use the shared exception transition"
    );
    assert!(
        direct_calls > 0,
        "the TDZ throw must cross a compiled direct-call boundary"
    );
}
