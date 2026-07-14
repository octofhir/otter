//! Production template-tier collection-allocation coverage.
//!
//! # Contents
//! - `new Map`/`new Set` (NewCollection), `new WeakRef`, and
//!   `new FinalizationRegistry` from loop OSR.
//!
//! # Invariants
//! - The allocation opcodes complete in machine code through the shared
//!   reentrant construction transition; every result matches the interpreter
//!   oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_construct_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function collections(rounds) {
  let acc = 0;
  for (let round = 0; round < rounds; round++) {
    const m = new Map([["k", round]]);
    acc += m.get("k");
    const s = new Set([round, round + 1]);
    acc += s.size;
    const w = new WeakRef({ v: round });
    acc += w.deref().v;
    const fr = new FinalizationRegistry(() => {});
    acc += fr ? 1 : 0;
  }
  return acc;
}

collections(180);
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
            "jit-collections.js",
        )
        .expect("collections matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (completion, stats.jit_osr_attempts)
}

#[test]
fn collection_allocation_matches_oracle() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
}
