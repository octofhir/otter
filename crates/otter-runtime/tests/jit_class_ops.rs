//! Production template-tier class-construction coverage.
//!
//! # Contents
//! - Derived-constructor `super()` binding (BindThisValue) from a hot
//!   instantiation loop; class heritage/naming validated by the conformance
//!   gates.
//!
//! # Invariants
//! - `BindThisValue` completes in machine code through the shared reentrant
//!   transition; every result matches the interpreter oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_class_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function classes(rounds) {
  class Base {
    greet() { return 1; }
  }
  class Derived extends Base {
    constructor(n) {
      super();
      this.n = n;
    }
    total() {
      return this.n + this.greet();
    }
  }
  let acc = 0;
  for (let round = 0; round < rounds; round++) {
    const d = new Derived(round);
    acc += d.total();
  }
  return acc;
}

classes(180);
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
            "jit-class-ops.js",
        )
        .expect("class matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (completion, stats.jit_osr_attempts)
}

#[test]
fn class_ops_match_oracle() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
}
