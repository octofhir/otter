//! Ordinary intrinsic calls and construction across Template loop OSR.
//!
//! # Contents
//! - `Array.of`, `Array.from`, `new Array`, and `queueMicrotask` from loop OSR.
//!
//! # Invariants
//! - The compiler resolves actual callables before arguments; admitted intrinsic
//!   forms use their identity proof and other calls keep ordinary completion.
//! - Every result matches the interpreter oracle after loop OSR.
//!
//! # See also
//! - `otter_vm::JitNativeCall` and the source-owned intrinsic proof.

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
        .jit_compile_threads(0)
        .jit_selection(selection)
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
fn ordinary_intrinsic_calls_match_oracle_after_osr() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
}
