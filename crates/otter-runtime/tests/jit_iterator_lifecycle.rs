//! Production template-tier iterator lifecycle coverage.
//!
//! # Contents
//! - Array and user iterator `next` completion from loop OSR.
//! - Abrupt `break` close and closer-registry start/end transitions.
//! - Getter/callback reentry without replaying a committed iterator opcode.
//!
//! # Invariants
//! - `GetIterator` (built-in and user `[Symbol.iterator]()`) completes in
//!   machine code through the shared reentrant transition; the compiled body
//!   no longer side-exits at iterator acquisition.
//! - The observable close counter matches the interpreter oracle exactly.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_iterator_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function arraySum(rounds) {
  let total = 0;
  for (let round = 0; round < rounds; round++) {
    for (const value of [1, 2, 3, 4]) total += value;
  }
  return total;
}

function closeOnBreak(rounds) {
  let nextCalls = 0;
  let closeCalls = 0;
  const iterable = {
    [Symbol.iterator]() {
      let value = 0;
      return {
        next() {
          nextCalls++;
          return { value: value++, done: false };
        },
        return() {
          closeCalls++;
          return { done: true };
        }
      };
    }
  };
  let total = 0;
  for (let round = 0; round < rounds; round++) {
    for (const value of iterable) {
      total += value;
      break;
    }
  }
  return [total, nextCalls, closeCalls].join(":");
}

JSON.stringify([arraySum(180), closeOnBreak(180)]);
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
            "jit-iterator-lifecycle.js",
        )
        .expect("iterator matrix")
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
fn iterator_lifecycle_completes_from_loop_osr() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "iterator stepping and close must use the shared reentrant transition"
    );
}
