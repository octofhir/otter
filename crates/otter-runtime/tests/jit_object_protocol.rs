//! Production template-tier object property-protocol coverage.
//!
//! # Contents
//! - `instanceof`, `in`, and `Object.getPrototypeOf`/`setPrototypeOf` from
//!   loop OSR over ordinary objects.
//! - A Proxy whose `has`/`getPrototypeOf` traps are observable during the
//!   compiled transition.
//!
//! # Invariants
//! - Each protocol opcode completes in machine code through the shared
//!   reentrant transition; the compiled body no longer side-exits.
//! - Trap call counts and every protocol result match the interpreter oracle.
//!
//! # See also
//! - `otter_vm::Interpreter::jit_runtime_object_protocol_op`

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function ordinary(rounds) {
  class Base {}
  const obj = new Base();
  obj.field = 1;
  let hits = 0;
  for (let round = 0; round < rounds; round++) {
    if (obj instanceof Base) hits++;
    if ("field" in obj) hits++;
    if (Object.getPrototypeOf(obj) === Base.prototype) hits++;
  }
  return hits;
}

function proxied(rounds) {
  let hasCalls = 0;
  let protoCalls = 0;
  const target = {};
  const proto = {};
  const p = new Proxy(target, {
    has(t, k) { hasCalls++; return k === "yes"; },
    getPrototypeOf() { protoCalls++; return proto; },
  });
  let hits = 0;
  for (let round = 0; round < rounds; round++) {
    if ("yes" in p) hits++;
    if (Object.getPrototypeOf(p) === proto) hits++;
  }
  return [hits, hasCalls, protoCalls].join(":");
}

JSON.stringify([ordinary(180), proxied(180)]);
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
            "jit-object-protocol.js",
        )
        .expect("protocol matrix")
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
fn object_protocol_completes_from_loop_osr() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    let (compiled, osr_attempts, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "protocol queries must use the shared reentrant transition"
    );
}
