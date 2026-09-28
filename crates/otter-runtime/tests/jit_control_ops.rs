//! Production template-tier nullish-branch and eval-extension lookup
//! coverage.
//!
//! # Contents
//! - `??` null/undefined branching from loop OSR.
//! - A direct-eval `var` shadowing an outer captured binding through the
//!   callee's eval extension (`LoadLookupSlot`).
//! - An observable getter proving the nullish condition is evaluated once.
//!
//! # Invariants
//! - `JumpIfNullish` stays inline while `LoadLookupSlot` completes through
//!   the shared reentrant binding transition.
//! - Template execution matches the interpreter oracle exactly.
//!
//! # See also
//! - `otter_vm::context_ops` — the `Lookup*` kernels.

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
    assert_eq!(compiled, "3240:180:3");
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "LoadLookupSlot must use the shared reentrant transition"
    );
}
