//! Production template-tier global-variable access coverage.
//!
//! # Contents
//! - Plain global-var read/write inside a hot loop from loop OSR.
//! - Accessor global whose setter is observable during a compiled store.
//!
//! # Invariants
//! - `StoreGlobalBinding`/`LoadGlobalOrThrow` complete in machine code through
//!   the shared reentrant global transition; the compiled body no longer
//!   side-exits at global access.
//! - The accessor setter call count and every global value match the
//!   interpreter oracle exactly.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_global_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
var acc = 0;
var setterCalls = 0;
globalThis._sensor = 0;
Object.defineProperty(globalThis, "sensor", {
  get() { return this._sensor; },
  set(v) { setterCalls++; this._sensor = v; },
  configurable: true,
});

function run(rounds) {
  for (let round = 0; round < rounds; round++) {
    acc = acc + round;
    sensor = acc;
  }
  return [acc, sensor, setterCalls].join(":");
}

run(180);
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
            "jit-global-access.js",
        )
        .expect("global matrix")
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
fn global_access_completes_from_loop_osr() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "global access must use the shared reentrant transition"
    );
}
