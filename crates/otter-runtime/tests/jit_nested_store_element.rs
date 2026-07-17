//! Frameless direct-callee `StoreElement` semantic coverage.
//!
//! # Contents
//! - A hot compiled caller entering a hot compiled element-store callee.
//! - Computed-key coercion and an inherited allocating setter on an object.
//! - Array growth and typed-array value coercion through the same callee.
//!
//! # Invariants
//! - Loop OSR is disabled: the store executes in a frameless whole-function
//!   direct callee rather than a materialized interpreter activation.
//! - Every supported `[[Set]]` path completes in place without deoptimization.
//! - Interpreter and production-tier observable results remain identical.

use otter_runtime::{JitSelection, Runtime, SourceInput};

struct FinalStats {
    osr_attempts: u64,
    direct_calls: u64,
    property_stubs: u64,
    optimized_deopts: u64,
}

const SETUP: &str = r#"
function storeElement(receiver, key, value) {
  receiver[key] = value;
  return receiver[key];
}

function callStoreElement(receiver, key, value) {
  return storeElement(receiver, key, value);
}

class StampBase {
  constructor(receiver) { return receiver; }
}
class Stamper extends StampBase {
  #value = 1;
}

globalThis.warmStoreReceiver = { slot: 0 };
for (let i = 0; i < 5000; i++) {
  callStoreElement(warmStoreReceiver, "slot", i);
  new Stamper({});
}
"#;

const FINAL: &str = r#"
globalThis.setterCalls = 0;
globalThis.setterValue = 0;
globalThis.setterAllocations = 0;
const setterPrototype = {};
Object.defineProperty(setterPrototype, "slot", {
  configurable: true,
  get() { return setterValue; },
  set(value) {
    setterCalls++;
    setterValue = value;
    const garbage = [];
    for (let i = 0; i < 64; i++) garbage.push({ i, value });
    setterAllocations += garbage.length;
  }
});
globalThis.setterReceiver = Object.create(setterPrototype);

globalThis.keyCoercions = 0;
globalThis.storeKey = {
  [Symbol.toPrimitive]() {
    keyCoercions++;
    return "slot";
  }
};

globalThis.grownStoreArray = [];
globalThis.typedStoreArray = new Int32Array(4);
globalThis.typedValueCoercions = 0;
globalThis.typedStoreValue = {
  valueOf() {
    typedValueCoercions++;
    return 17.75;
  }
};

globalThis.primitiveSetterValue = 0;
Object.defineProperty(Number.prototype, "nestedStore", {
  configurable: true,
  get() { return primitiveSetterValue; },
  set(value) { primitiveSetterValue += value; }
});

globalThis.proxyStoreCalls = 0;
globalThis.proxyStoreTarget = {};
globalThis.proxyStoreReceiver = new Proxy(proxyStoreTarget, {
  set(target, key, value, receiver) {
    proxyStoreCalls++;
    return Reflect.set(target, key, value, receiver);
  }
});

globalThis.nestedStoreResults = [
  callStoreElement(setterReceiver, storeKey, 41),
  callStoreElement(grownStoreArray, 8, 42.5),
  callStoreElement(typedStoreArray, "1", typedStoreValue),
  callStoreElement(1, "nestedStore", 9),
  callStoreElement(proxyStoreReceiver, "proxied", 23)
];
delete Number.prototype.nestedStore;

globalThis.privateStoreError = "";
try {
  new Stamper(Object.preventExtensions({}));
} catch (error) {
  privateStoreError = error.name;
}
"#;

const OBSERVE: &str = r#"
JSON.stringify([
  nestedStoreResults,
  setterCalls,
  setterValue,
  setterAllocations,
  keyCoercions,
  grownStoreArray.length,
  0 in grownStoreArray,
  grownStoreArray[8],
  typedStoreArray[1],
  typedValueCoercions,
  primitiveSetterValue,
  proxyStoreCalls,
  proxyStoreTarget.proxied,
  privateStoreError
]);
"#;

fn run(selection: JitSelection) -> (String, FinalStats) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(SETUP),
            "jit-nested-store-element-setup.js",
        )
        .expect("store warmup");
    let before = runtime.execution_stats();
    runtime
        .run_script(
            SourceInput::from_javascript(FINAL),
            "jit-nested-store-element-final.js",
        )
        .expect("nested stores");
    let after = runtime.execution_stats();
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(OBSERVE),
            "jit-nested-store-element-observe.js",
        )
        .expect("observe nested stores")
        .completion_string()
        .to_owned();
    (
        completion,
        FinalStats {
            osr_attempts: after.jit_osr_attempts - before.jit_osr_attempts,
            direct_calls: after.jit_direct_calls - before.jit_direct_calls,
            property_stubs: after.jit_runtime_property_stubs - before.jit_runtime_property_stubs,
            optimized_deopts: after.jit_optimized_deopts - before.jit_optimized_deopts,
        },
    )
}

#[test]
fn frameless_store_element_completes_object_array_and_typed_array_set() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, stats) = run(JitSelection::ProductionTiered);

    assert_eq!(compiled, oracle);
    assert_eq!(
        oracle,
        "[[41,42.5,17,9,23],1,41,64,2,9,false,42.5,17,1,9,1,23,\"TypeError\"]"
    );
    assert_eq!(stats.osr_attempts, 0);
    assert!(
        stats.direct_calls > 0,
        "final stores must cross a compiled caller-to-callee boundary"
    );
    assert!(
        stats.property_stubs > 0,
        "computed stores must cross the shared runtime [[Set]] boundary"
    );
    assert_eq!(
        stats.optimized_deopts, 0,
        "supported computed stores must not materialize or deopt"
    );
}
