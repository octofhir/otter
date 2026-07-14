//! Production template-tier intrinsic-call coverage.
//!
//! # Contents
//! - `ArrayBuffer`, `DataView`, `BigInt`, and `Promise.resolve` construction
//!   from loop OSR.
//!
//! # Invariants
//! - The intrinsic-call opcodes complete in machine code through the shared
//!   reentrant transitions; every result matches the interpreter oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_variadic_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function intrinsics(rounds) {
  let acc = 0;
  for (let round = 0; round < rounds; round++) {
    const ab = new ArrayBuffer(8);
    acc += ab.byteLength;
    const b = BigInt(round);
    acc += Number(b);
    const dv = new DataView(ab);
    acc += dv.byteLength;
    const p = Promise.resolve(round);
    acc += p ? 1 : 0;
  }
  return acc;
}

intrinsics(180);
"#;

fn run(selection: JitSelection) -> (String, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(8)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-intrinsic-calls.js",
        )
        .expect("intrinsic matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (completion, stats.jit_osr_attempts)
}

#[test]
fn intrinsic_calls_match_oracle() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
}
