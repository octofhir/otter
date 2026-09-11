//! Template calls without a generated edge complete in place.
//!
//! # Contents
//! - A hot function whose plain call site alternates between two closures,
//!   so no monomorphic direct-call plan exists.
//! - A plain call and a receiver call with six arguments, wider than the
//!   four-lane direct packing.
//!
//! # Invariants
//! - Every tier returns the interpreter's completion.
//! - Sites without a generated edge run the variadic in-place transition and
//!   never side-exit the compiled body; the generated body keeps completing.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function add(a, b) { return a + b; }
function mul(a, b) { return a * b; }
function wide(a, b, c, d, e, f) { return a + b * 2 + c * 3 + d * 4 + e * 5 + f * 6; }
const box = { wide: wide, base: 1 };

function poly(rounds) {
  let acc = 0;
  for (let i = 0; i < rounds; i++) {
    const fn = (i & 1) === 0 ? add : mul;
    acc += fn(i, 3);
  }
  return acc;
}
function wideCalls(rounds) {
  let acc = 0;
  for (let i = 0; i < rounds; i++) {
    acc += wide(i, 1, 2, 3, 4, 5);
    acc += box.wide(box.base, i, 2, 3, 4, 5);
  }
  return acc;
}
poly(3000) + ";" + wideCalls(3000);
"#;

fn run(selection: JitSelection) -> (String, u64, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(16)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-generic-call-transition.js",
        )
        .expect("calls without a generated edge")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (
        completion,
        stats.jit_to_rust_call_transitions,
        stats.jit_osr_attempts,
    )
}

#[test]
fn calls_without_a_generated_edge_match_the_interpreter_on_every_tier() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let (compiled, _, _) = run(selection);
        assert_eq!(compiled, oracle, "{selection:?}");
    }
}

#[test]
fn calls_without_a_generated_edge_complete_in_place() {
    let (_, transitions, osr_attempts) = run(JitSelection::Template);
    assert!(osr_attempts > 0, "the loops must enter compiled code");
    // Both loops run thousands of iterations inside compiled bodies; each
    // unplanned call crosses the in-place transition once instead of leaving
    // the body, so the transition count tracks the call count.
    assert!(
        transitions >= 2000,
        "unplanned calls must complete through the in-place transition: {transitions}"
    );
}
