//! Production template-tier `Function.prototype.bind` coverage.
//!
//! # Contents
//! - Plain bound-argument capture and invocation from loop OSR.
//! - Accessor `name` getter reentry during bind without replaying the opcode.
//!
//! # Invariants
//! - `BindFunction` completes in machine code through the shared reentrant
//!   transition; the compiled body no longer side-exits at bind.
//! - The observable getter-call counter and every bound result match the
//!   interpreter oracle exactly.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_bind_function`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function plainBind(rounds) {
  function target(a, b) { return this.base + a + b; }
  const ctx = { base: 10 };
  let sum = 0;
  for (let round = 0; round < rounds; round++) {
    const bound = target.bind(ctx, round);
    sum += bound(1);
  }
  return sum;
}

function accessorName(rounds) {
  let nameReads = 0;
  const target = function (a) { return a * 2; };
  Object.defineProperty(target, "name", {
    get() { nameReads++; return "dyn"; },
    configurable: true,
  });
  let out = 0;
  for (let round = 0; round < rounds; round++) {
    const bound = target.bind(null, round);
    out += bound();
  }
  const boundName = target.bind(null).name;
  return [out, nameReads, boundName].join(":");
}

JSON.stringify([plainBind(180), accessorName(180)]);
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
            "jit-bind-function.js",
        )
        .expect("bind matrix")
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
fn bind_function_completes_from_loop_osr() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "bind completion must use the shared reentrant transition"
    );
}
