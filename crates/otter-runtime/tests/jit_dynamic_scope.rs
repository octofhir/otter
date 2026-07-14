//! Production template-tier dynamic-scope name access coverage.
//!
//! # Contents
//! - `with`-block `LoadDynamic`, `StoreDynamic`, and `TypeofDynamic` from loop
//!   OSR.
//!
//! # Invariants
//! - Dynamic name access completes in machine code through the shared reentrant
//!   global/environment transition; the compiled body no longer side-exits.
//! - Every result matches the interpreter oracle exactly.
//!
//! # See also
//! - `otter_vm::Interpreter::run_load_dynamic_reg`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function dynamicScope(rounds) {
  let acc = 0;
  const scope = { val: 0, flag: true };
  for (let round = 0; round < rounds; round++) {
    with (scope) {
      val = round;
      acc += val;
      if (typeof flag === "boolean") acc++;
    }
  }
  return acc;
}

dynamicScope(180);
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
            "jit-dynamic-scope.js",
        )
        .expect("dynamic matrix")
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
fn dynamic_scope_matches_oracle() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, _osr, _reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
}
