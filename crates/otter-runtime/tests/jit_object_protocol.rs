//! Production template-tier object property-protocol coverage.
//!
//! # Contents
//! - `instanceof`, `in`, and `Object.getPrototypeOf`/`setPrototypeOf` from
//!   loop OSR over ordinary objects.
//! - A Proxy whose `has`/`getPrototypeOf` traps are observable during the
//!   compiled transition.
//! - Young bound targets, Proxy operands, ToPropertyKey hooks, and prototype
//!   traps surviving allocation at every moving-GC stress stride.
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

const ROOTED_REENTRY_SOURCE: &str = r#"
let allocations = 0;
function churn(seed) {
  let tail = null;
  for (let i = 0; i < 48; i++) {
    tail = { seed, i, text: "protocol-" + seed + "-" + i, tail };
    allocations++;
  }
  return tail;
}

let hasInstanceGets = 0;
let hasInstanceCalls = 0;
let candidateGets = 0;
function Target() {}
Object.defineProperty(Target, Symbol.hasInstance, {
  configurable: true,
  get() {
    hasInstanceGets++;
    churn(1000 + hasInstanceGets);
    return function(candidate) {
      hasInstanceCalls++;
      churn(2000 + hasInstanceCalls);
      return candidate.marker === 41;
    };
  }
});
const Bound = Target.bind(null);
const candidate = new Proxy({ marker: 41 }, {
  get(target, key, receiver) {
    candidateGets++;
    churn(3000 + candidateGets);
    return Reflect.get(target, key, receiver);
  }
});

let keyCalls = 0;
let hasCalls = 0;
const dynamicKey = {
  toString() {
    keyCalls++;
    churn(4000 + keyCalls);
    return "yes";
  }
};
const hasProxy = new Proxy({ yes: true }, {
  has(target, key) {
    hasCalls++;
    churn(5000 + hasCalls);
    return Reflect.has(target, key);
  }
});

let getPrototypeCalls = 0;
let setPrototypeCalls = 0;
const expectedPrototype = { expected: true };
const prototypeProxy = new Proxy({}, {
  getPrototypeOf() {
    getPrototypeCalls++;
    churn(6000 + getPrototypeCalls);
    return expectedPrototype;
  }
});
const setPrototypeTarget = {};
const setPrototypeProxy = new Proxy(setPrototypeTarget, {
  setPrototypeOf(target, prototype) {
    setPrototypeCalls++;
    churn(7000 + setPrototypeCalls);
    return Reflect.setPrototypeOf(target, prototype);
  }
});

function rootedProtocol(rounds) {
  let hits = 0;
  for (let round = 0; round < rounds; round++) {
    if (candidate instanceof Bound) hits++;
    if (dynamicKey in hasProxy) hits++;
    if (Object.getPrototypeOf(prototypeProxy) === expectedPrototype) hits++;
    Object.setPrototypeOf(setPrototypeProxy, expectedPrototype);
    if (Object.getPrototypeOf(setPrototypeTarget) === expectedPrototype) hits++;
  }
  return hits;
}

const rounds = 72;
JSON.stringify([
  rootedProtocol(rounds),
  hasInstanceGets, hasInstanceCalls, candidateGets,
  keyCalls, hasCalls, getPrototypeCalls, setPrototypeCalls,
  allocations > 0
]);
"#;

fn run_source(selection: JitSelection, source: &str, name: &str) -> (String, u64, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(8)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(SourceInput::from_javascript(source.to_string()), name)
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
    let (oracle, _, _) = run_source(
        JitSelection::InterpreterOnly,
        SOURCE,
        "jit-object-protocol.js",
    );
    let (compiled, osr_attempts, reentrant) =
        run_source(JitSelection::Template, SOURCE, "jit-object-protocol.js");
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant > 0,
        "protocol queries must use the shared reentrant transition"
    );
}

#[test]
fn committed_protocol_roots_young_proxy_and_bound_intermediates() {
    let (oracle, _, _) = run_source(
        JitSelection::InterpreterOnly,
        ROOTED_REENTRY_SOURCE,
        "jit-object-protocol-rooted-reentry.js",
    );
    let (compiled, osr_attempts, reentrant) = run_source(
        JitSelection::Template,
        ROOTED_REENTRY_SOURCE,
        "jit-object-protocol-rooted-reentry.js",
    );
    assert_eq!(compiled, oracle);
    assert_eq!(compiled, r#"[288,72,72,72,72,72,72,72,true]"#);
    assert!(osr_attempts > 0, "fixture must enter at a loop OSR header");
    assert!(
        reentrant >= 288,
        "every rooted protocol operation must cross the committed boundary"
    );
}
