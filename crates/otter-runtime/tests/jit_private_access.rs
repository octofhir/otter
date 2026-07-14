//! Production template-tier private-member access coverage.
//!
//! # Contents
//! - Private data field get/set, private accessor get/set, private method call,
//!   and a `#field in obj` brand check from loop OSR.
//!
//! # Invariants
//! - Private access completes in machine code through the shared reentrant
//!   transition; the compiled body no longer side-exits.
//! - Private accessor getter/setter effects match the interpreter oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_private_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function privates(rounds) {
  let getCalls = 0;
  let setCalls = 0;
  class C {
    #field = 0;
    get #acc() { getCalls++; return this.#field; }
    set #acc(v) { setCalls++; this.#field = v; }
    #method() { return 7; }
    run(rounds) {
      let acc = 0;
      for (let round = 0; round < rounds; round++) {
        this.#field = round;
        acc += this.#field;
        this.#acc = round;
        acc += this.#acc;
        acc += this.#method();
        if (#field in this) acc++;
      }
      return acc;
    }
  }
  return [new C().run(rounds), getCalls, setCalls].join(":");
}

privates(180);
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
            "jit-private-access.js",
        )
        .expect("private matrix")
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
fn private_access_completes_from_loop_osr() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "private access must use the shared reentrant transition"
    );
}
