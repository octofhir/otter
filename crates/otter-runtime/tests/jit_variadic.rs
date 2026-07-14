//! Production template-tier variadic-construction coverage.
//!
//! # Contents
//! - `Array.of`, `Array.from`, `new Array`, and `queueMicrotask` from loop OSR.
//!
//! # Invariants
//! - The variadic opcodes complete in machine code through the shared reentrant
//!   transition; every result matches the interpreter oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_variadic_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function variadics(rounds) {
  let acc = 0;
  let ticks = 0;
  for (let round = 0; round < rounds; round++) {
    const a = Array.of(round, round + 1, round + 2);
    acc += a.length + a[0];
    const b = Array.from([round, round + 1]);
    acc += b.length;
    const c = new Array(3);
    acc += c.length;
    queueMicrotask(() => { ticks++; });
  }
  return acc + ticks;
}

variadics(180);
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
            "jit-variadic.js",
        )
        .expect("variadic matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (completion, stats.jit_osr_attempts)
}

#[test]
fn variadic_ops_match_oracle() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
}
