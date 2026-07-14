//! Production template-tier `delete` coverage.
//!
//! # Contents
//! - Named and computed `delete` on ordinary objects from loop OSR.
//! - A Proxy whose `deleteProperty` trap is observable during the compiled
//!   transition.
//! - Unqualified `delete` of a configurable global.
//!
//! # Invariants
//! - `DeleteProperty`/`DeleteElement`/`DeleteDynamic` complete in machine code
//!   through the shared reentrant transition; the compiled body no longer
//!   side-exits at `delete`.
//! - The trap call count and every delete result match the interpreter oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_delete_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
globalThis.slot = 0;

function deletes(rounds) {
  let trapCalls = 0;
  let removed = 0;
  for (let round = 0; round < rounds; round++) {
    const obj = { a: 1, b: 2, 7: 3 };
    if (delete obj.a) removed++;
    if (delete obj[7]) removed++;
    const p = new Proxy(
      { x: 1 },
      {
        deleteProperty(t, k) {
          trapCalls++;
          delete t[k];
          return true;
        },
      },
    );
    if (delete p.x) removed++;
    globalThis.slot = round;
    if (delete slot) removed++;
  }
  return [removed, trapCalls].join(":");
}

deletes(180);
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
            "jit-delete.js",
        )
        .expect("delete matrix")
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
fn delete_completes_from_loop_osr() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "delete must use the shared reentrant transition"
    );
}
