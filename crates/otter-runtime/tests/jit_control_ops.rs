//! Production template-tier nullish-branch and shadowed-upvalue coverage.
//!
//! # Contents
//! - `??` null/undefined branching from loop OSR.
//! - A direct-eval `var` shadowing an outer captured binding.
//! - An observable getter proving the nullish condition is evaluated once.
//!
//! # Invariants
//! - `JumpIfNullish` stays inline while `LoadShadowedUpvalue` completes through
//!   the shared reentrant VM helper.
//! - Template execution matches the interpreter oracle exactly.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_control_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function control(rounds) {
  let captured = 3;
  let effects = 0;
  let acc = 0;

  function hot(count) {
    eval("var captured = 11");
    for (let round = 0; round < count; round++) {
      const box = {
        get value() {
          effects++;
          if (round % 3 === 0) return null;
          if (round % 3 === 1) return undefined;
          return captured;
        }
      };
      acc += box.value ?? 5;
      acc += captured;
    }
  }

  hot(rounds);
  return acc + ":" + effects + ":" + captured;
}

control(180);
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
            "jit-control-ops.js",
        )
        .expect("control matrix")
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
fn control_ops_match_oracle_with_single_getter_evaluation() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert_eq!(compiled, "2760:180:3");
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "LoadShadowedUpvalue must use the shared reentrant transition"
    );
}
