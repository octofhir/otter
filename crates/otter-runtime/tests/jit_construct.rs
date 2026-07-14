//! Production template-tier allocating-construction coverage.
//!
//! # Contents
//! - Rest-parameter collection, `new Error`, and array spread/push from loop
//!   OSR.
//!
//! # Invariants
//! - The exercised construction opcodes complete in machine code through the
//!   shared reentrant transition; every result matches the interpreter oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_construct_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function constructs(rounds) {
  function rest(...args) {
    return args.length + args[0];
  }
  let acc = 0;
  for (let round = 0; round < rounds; round++) {
    acc += rest(round, round + 1, round + 2);
    const spread = [round, ...[round + 1, round + 2]];
    acc += spread.length;
    try {
      throw new Error("boom" + round);
    } catch (e) {
      acc += e.message.length;
    }
  }
  return acc;
}

constructs(180);
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
            "jit-construct.js",
        )
        .expect("construct matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (completion, stats.jit_osr_attempts)
}

#[test]
fn construct_ops_match_oracle() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
}
