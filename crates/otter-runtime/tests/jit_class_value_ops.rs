//! Production template-tier class and dynamic-value coverage.
//!
//! # Contents
//! - Class/private-name creation, tagged-template caching, direct eval, dynamic
//!   `Function`, eval identity, and full object `ToNumber` in one hot loop.
//! - Observable `valueOf`/`toString`/tag side effects proving single execution.
//! - Interpreter/template completion parity.
//!
//! # Invariants
//! - Reentrant coercion and dynamic compilation complete exactly once.
//! - Template output is byte-identical to the interpreter oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_class_value_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
let effects = 0;
let cachedTemplate;
const crossChunk = Function(
  "i",
  "class Inner { #value; constructor(value) { this.#value = value; } get() { return this.#value; } } function localTag(strings, value) { return value; } return localTag`value=${i}` + new Inner(i).get();"
);

function tag(strings, value) {
  if (cachedTemplate === undefined) {
    cachedTemplate = strings;
    effects += 1;
  } else {
    effects += strings === cachedTemplate ? 2 : 1000;
  }
  return value;
}

function matrix(rounds) {
  let total = 0;
  for (let i = 0; i < rounds; i++) {
    const numberLike = {
      valueOf() {
        effects++;
        return "40";
      }
    };
    const parameter = {
      toString() {
        effects++;
        return "x";
      }
    };
    const generated = Function(parameter, "return x + 1");
    class Box {
      #value;
      constructor(value) { this.#value = value; }
      get() { return this.#value; }
    }
    const tagged = tag`value=${i}`;
    const evaluated = eval("2 + 3");
    total += +numberLike + generated(i) + new Box(i).get() + tagged + evaluated + crossChunk(i);
  }
  return JSON.stringify([total, effects]);
}

matrix(80);
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
            "jit-class-value-ops.js",
        )
        .expect("class/value matrix")
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
fn class_value_ops_match_oracle_with_single_coercion_effects() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert_eq!(compiled, "[19480,319]");
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "class/value opcodes must use the shared reentrant transition"
    );
}
