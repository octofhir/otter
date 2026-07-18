//! Production-tier `StoreProperty` miss completion coverage.
//!
//! # Contents
//! - Interpreter/template parity for own data, shape transitions, accessors,
//!   non-writable inheritance, proxies, exotics, primitives, and megamorphism.
//! - A polymorphic shape miss whose float RHS would allocate during boxing.
//! - Reentrant/allocating setters and throwing-setter effect/PC ordering.
//!
//! # Invariants
//! - Every scenario executes the store after loop OSR has entered compiled code.
//! - Rejected store guards remain allocation-free, so fallback never observes
//!   a receiver handle forwarded by RHS boxing.
//! - A committed setter effect is observed exactly once; the opcode is never
//!   replayed after an in-place slow transition.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
let own = { x: 0 };
function ownHit(o) {
  for (let i = 0; i < 8; i++) o.x = i;
  return o.x;
}

let transitionObjects = [
  {}, {a: 1}, {b: 1}, {c: 1}, {d: 1}, {e: 1}, {f: 1}, {g: 1}
];
function shapeTransitions(items) {
  for (let i = 0; i < items.length; i++) items[i].x = i;
  return items[7].x;
}

let floatStoreA = { a: 0 };
let floatStoreB = { b: 0 };
function polymorphicFloatStore(o, value) {
  for (let i = 0; i < 8; i++) o.x = value + i * 0.25;
  return o.x;
}

let setterCalls = 0;
let setterSum = 0;
let setterProto = {
  set x(v) {
    setterCalls++;
    setterSum += v;
  }
};
let setterReceiver = Object.create(setterProto);
function inheritedSetter(o) {
  for (let i = 0; i < 6; i++) o.x = i;
}

let throwCalls = 0;
let throwProgress = 0;
let throwingReceiver = Object.create({
  set x(v) {
    throwCalls++;
    if (v === 3) throw new Error("setter boom");
  }
});
function throwingSetter(o) {
  for (let i = 0; i < 6; i++) {
    o.x = i;
    throwProgress++;
  }
}

let blockedProto = {};
Object.defineProperty(blockedProto, "x", {
  value: 1,
  writable: false,
  configurable: false
});
let writableReceiver = { x: 0 };
let blockedReceiver = Object.create(blockedProto);
function inheritedReadonly(ok, blocked) {
  "use strict";
  for (let i = 0; i < 6; i++) (i < 3 ? ok : blocked).x = i;
}

let proxyCalls = 0;
let proxyTarget = {};
let proxy = new Proxy(proxyTarget, {
  set(target, key, value, receiver) {
    proxyCalls++;
    return Reflect.set(target, key, value, receiver);
  }
});
function proxyStore(o) {
  for (let i = 0; i < 6; i++) o.x = i;
}

let exotic = /x/;
function exoticStore(o) {
  for (let i = 0; i < 6; i++) o.lastIndex = i;
}

let primitiveSetterCalls = 0;
Object.defineProperty(Number.prototype, "jitStore", {
  configurable: true,
  set(v) { primitiveSetterCalls += v; }
});
function primitiveStore() {
  for (let i = 0; i < 6; i++) (1).jitStore = i;
}

let reentryCalls = 0;
let reentryAllocs = 0;
function allocateInSetter(v) {
  let items = [];
  for (let i = 0; i < 96; i++) items.push({i, v, text: "gc-" + i});
  reentryAllocs += items.length;
}
let reentryReceiver = Object.create({
  set x(v) {
    reentryCalls++;
    allocateInSetter(v);
  }
});
function reentrantSetter(o) {
  for (let i = 0; i < 6; i++) o.x = i;
}

ownHit(own);
shapeTransitions(transitionObjects);
let floatStoreFirst = polymorphicFloatStore(floatStoreA, 1.25);
let floatStoreMiss = polymorphicFloatStore(floatStoreB, 2.5);
inheritedSetter(setterReceiver);
let throwName = "";
try { throwingSetter(throwingReceiver); } catch (e) { throwName = e.message; }
let readonlyName = "";
try { inheritedReadonly(writableReceiver, blockedReceiver); }
catch (e) { readonlyName = e.name; }
proxyStore(proxy);
exoticStore(exotic);
primitiveStore();
reentrantSetter(reentryReceiver);
delete Number.prototype.jitStore;

JSON.stringify([
  own.x,
  transitionObjects[7].x,
  floatStoreFirst,
  floatStoreMiss,
  setterCalls,
  setterSum,
  throwCalls,
  throwProgress,
  throwName,
  readonlyName,
  Object.prototype.hasOwnProperty.call(blockedReceiver, "x"),
  proxyCalls,
  proxyTarget.x,
  exotic.lastIndex,
  primitiveSetterCalls,
  reentryCalls,
  reentryAllocs
]);
"#;

fn run(selection: JitSelection) -> (String, u64, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(1)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SOURCE.to_string()),
            "jit-store-property.js",
        )
        .expect("store-property matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (
        completion,
        stats.jit_osr_attempts,
        stats.jit_runtime_property_stubs,
    )
}

#[test]
fn store_property_misses_complete_in_place_with_interpreter_parity() {
    let (oracle, _, _) = run(JitSelection::InterpreterOnly);
    assert_eq!(
        oracle,
        "[7,7,3,4.25,6,15,4,3,\"setter boom\",\"TypeError\",false,6,5,5,15,6,576]"
    );
    let (compiled, osr_attempts, property_stubs) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr_attempts > 0, "store scenarios must request loop OSR");
    assert!(
        property_stubs > 0,
        "compiled StoreProperty must execute its runtime transition"
    );
}
