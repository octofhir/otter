//! Production template-tier static value-load coverage.
//!
//! # Contents
//! - `Math.*`, `Symbol.*`, BigInt literal, and string-index loads from loop OSR.
//!
//! # Invariants
//! - The exercised load opcodes complete in machine code through the shared
//!   reentrant transition; every result matches the interpreter oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_value_load_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function loads(rounds) {
  let acc = 0;
  const text = "otter";
  for (let round = 0; round < rounds; round++) {
    acc += Math.PI > 3 ? 1 : 0;
    if (Symbol.iterator) acc++;
    acc += Number(10n % 3n);
    acc += text[round % 5].charCodeAt(0) & 1;
  }
  return acc;
}

loads(180);
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
            "jit-value-load.js",
        )
        .expect("value-load matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (completion, stats.jit_osr_attempts)
}

#[test]
fn value_loads_match_oracle() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
}
