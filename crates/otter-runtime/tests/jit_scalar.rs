//! Production template-tier scalar value-query / coercion coverage.
//!
//! # Contents
//! - `typeof`, string/array length, `Array.isArray`, `Object.is`, and a
//!   computed class field key from loop OSR.
//!
//! # Invariants
//! - The exercised scalar opcodes complete in machine code through the shared
//!   reentrant transition; the compiled body no longer side-exits at them.
//! - Every result matches the interpreter oracle exactly.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_scalar_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function scalars(rounds) {
  const arr = [1, 2, 3];
  const text = "otter";
  let acc = 0;
  for (let round = 0; round < rounds; round++) {
    if (typeof arr === "object") acc++;
    if (typeof text === "string") acc++;
    if (Array.isArray(arr)) acc++;
    if (Object.is(round, round)) acc++;
    acc += text.length;
    const key = round % 2 === 0 ? "a" : "b";
    const obj = { [key]: round };
    acc += obj[key];
  }
  return acc;
}

scalars(180);
"#;

fn run(selection: JitSelection) -> (String, u64, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(8)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-scalar.js",
        )
        .expect("scalar matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (
        completion,
        stats.jit_osr_attempts,
        stats.jit_reentrant_stub_transitions,
    )
}

#[test]
fn scalar_ops_complete_from_loop_osr() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "scalar ops must use the shared reentrant transition"
    );
}
