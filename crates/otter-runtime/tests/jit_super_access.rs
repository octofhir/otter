//! Production template-tier `super` property access coverage.
//!
//! # Contents
//! - `super.prop` accessor get/set, `super.method()`, and computed
//!   `super[key]` from loop OSR inside a derived-class method.
//!
//! # Invariants
//! - Super access completes in machine code through the shared reentrant
//!   transition; the compiled body no longer side-exits.
//! - Home-prototype accessor getter/setter effects match the interpreter
//!   oracle exactly.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_super_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function supers(rounds) {
  let getCalls = 0;
  let setCalls = 0;
  class Base {
    get prop() { getCalls++; return this._p; }
    set prop(v) { setCalls++; this._p = v; }
    method() { return 10; }
  }
  class Derived extends Base {
    constructor() {
      super();
      this._p = 0;
    }
    run(rounds) {
      let acc = 0;
      for (let round = 0; round < rounds; round++) {
        super.prop = round;
        acc += super.prop;
        acc += super.method();
        const key = "method";
        acc += super[key]();
      }
      return acc;
    }
  }
  return [new Derived().run(rounds), getCalls, setCalls].join(":");
}

supers(180);
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
            "jit-super-access.js",
        )
        .expect("super matrix")
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
fn super_access_completes_from_loop_osr() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "super access must use the shared reentrant transition"
    );
}
