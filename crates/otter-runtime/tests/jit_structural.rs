//! Production template-tier structural object coverage.
//!
//! # Contents
//! - `for-in` key enumeration and object-spread copy from loop OSR.
//!
//! # Invariants
//! - `ForInKeys`/`CopyDataProperties` complete in machine code through the
//!   shared reentrant transition; every result matches the interpreter oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_structural_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function structural(rounds) {
  const base = { a: 1, b: 2, c: 3 };
  let acc = 0;
  for (let round = 0; round < rounds; round++) {
    for (const key in base) {
      acc += base[key];
    }
    const copy = { ...base, d: round };
    acc += copy.d + copy.a;
  }
  return acc;
}

structural(180);
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
            "jit-structural.js",
        )
        .expect("structural matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (completion, stats.jit_osr_attempts)
}

#[test]
fn structural_ops_match_oracle() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
}
