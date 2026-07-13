//! Production-tier property-load and coercion miss completion coverage.
//!
//! # Contents
//! - Full `[[Get]]` completion for accessors, proxies, primitive receivers,
//!   megamorphic sites, exceptions, and allocating getter reentry.
//! - In-place `ToPrimitive`/`ToNumeric` completion through observable
//!   `@@toPrimitive` and `valueOf` hooks.
//!
//! # Invariants
//! - Every matrix runs through loop OSR in the production template tier.
//! - A throwing getter/coercion hook advances no later instruction and is
//!   never replayed after its observable counter increment.
//! - Interpreter and compiled completion values remain identical under GC.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const LOAD_SOURCE: &str = r#"
let own = {x: 7};
function ownLoad(o) {
  let sum = 0;
  for (let i = 0; i < 6; i++) sum += o.x;
  return sum;
}

let shapes = [
  {x: 0}, {a: 1, x: 1}, {b: 1, x: 2}, {c: 1, x: 3},
  {d: 1, x: 4}, {e: 1, x: 5}, {f: 1, x: 6}, {g: 1, x: 7}
];
function polymorphicLoad(items) {
  let sum = 0;
  for (let i = 0; i < items.length; i++) sum += items[i].x;
  return sum;
}

let getterCalls = 0;
let inherited = Object.create({
  get x() { getterCalls++; return 5; }
});
function inheritedLoad(o) {
  let sum = 0;
  for (let i = 0; i < 6; i++) sum += o.x;
  return sum;
}

let throwCalls = 0;
let throwProgress = 0;
let throwing = Object.create({
  get x() {
    throwCalls++;
    if (throwCalls === 3) throw new Error("getter boom");
    return 1;
  }
});
function throwingLoad(o) {
  for (let i = 0; i < 6; i++) {
    o.x;
    throwProgress++;
  }
}

let proxyCalls = 0;
let proxy = new Proxy({x: 9}, {
  get(target, key, receiver) {
    proxyCalls++;
    return Reflect.get(target, key, receiver);
  }
});
function proxyLoad(o) {
  let sum = 0;
  for (let i = 0; i < 6; i++) sum += o.x;
  return sum;
}

let primitiveCalls = 0;
Object.defineProperty(Number.prototype, "jitLoad", {
  configurable: true,
  get() { primitiveCalls++; return 4; }
});
function primitiveLoad() {
  let sum = 0;
  for (let i = 0; i < 6; i++) sum += (1).jitLoad;
  return sum;
}

let reentryCalls = 0;
let reentryAllocs = 0;
let reentrant = Object.create({
  get x() {
    reentryCalls++;
    let values = [];
    for (let i = 0; i < 64; i++) values.push({i, text: "load-" + i});
    reentryAllocs += values.length;
    return 3;
  }
});
function reentrantLoad(o) {
  let sum = 0;
  for (let i = 0; i < 6; i++) sum += o.x;
  return sum;
}

let throwName = "";
let ownSum = ownLoad(own);
let shapeSum = polymorphicLoad(shapes);
let inheritedSum = inheritedLoad(inherited);
try { throwingLoad(throwing); } catch (e) { throwName = e.message; }
let proxySum = proxyLoad(proxy);
let primitiveSum = primitiveLoad();
let reentrySum = reentrantLoad(reentrant);
delete Number.prototype.jitLoad;

JSON.stringify([
  ownSum, shapeSum, inheritedSum, getterCalls,
  throwCalls, throwProgress, throwName,
  proxySum, proxyCalls, primitiveSum, primitiveCalls,
  reentrySum, reentryCalls, reentryAllocs
]);
"#;

const COERCION_SOURCE: &str = r#"
let symbolCalls = 0;
let symbolAllocs = 0;
let numberObject = {
  [Symbol.toPrimitive](hint) {
    symbolCalls++;
    if (hint !== "number") throw new Error("bad hint: " + hint);
    let values = [];
    for (let i = 0; i < 64; i++) values.push({i, text: "coerce-" + i});
    symbolAllocs += values.length;
    return "7";
  }
};
function subtractObject(o) {
  let sum = 0;
  for (let i = 0; i < 6; i++) sum += o - 2;
  return sum;
}

let valueOfCalls = 0;
let valueObject = {
  valueOf() { valueOfCalls++; return "4"; }
};
function multiplyObject(o) {
  let product = 1;
  for (let i = 0; i < 4; i++) product *= o;
  return product;
}

let throwCalls = 0;
let throwProgress = 0;
let throwing = {
  [Symbol.toPrimitive](hint) {
    throwCalls++;
    if (throwCalls === 3) throw new Error("coercion boom");
    return 1;
  }
};
function throwingCoercion(o) {
  for (let i = 0; i < 6; i++) {
    o - 0;
    throwProgress++;
  }
}

function primitiveNumeric(values) {
  let sum = 0;
  for (let i = 0; i < values.length; i++) sum += values[i] - 0;
  return sum;
}

let throwName = "";
let symbolType = "";
let sub = subtractObject(numberObject);
let product = multiplyObject(valueObject);
try { throwingCoercion(throwing); } catch (e) { throwName = e.message; }
try { Symbol("x") - 0; } catch (e) { symbolType = e.name; }
let primitiveSum = primitiveNumeric(["5", true, null, false]);

JSON.stringify([
  sub, symbolCalls, symbolAllocs,
  product, valueOfCalls,
  throwCalls, throwProgress, throwName,
  symbolType, primitiveSum
]);
"#;

fn run(source: &str, selection: JitSelection) -> (String, u64, u64, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(1)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(source.to_string()),
            "jit-load-coercion.js",
        )
        .expect("JIT completion matrix")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    (
        completion,
        stats.jit_osr_attempts,
        stats.jit_runtime_property_stubs,
        stats.jit_reentrant_stub_transitions,
    )
}

#[test]
fn property_load_misses_complete_in_place_with_exact_throw_order() {
    let (oracle, _, _, _) = run(LOAD_SOURCE, JitSelection::InterpreterOnly);
    assert_eq!(
        oracle,
        "[42,28,30,6,3,2,\"getter boom\",54,6,24,6,18,6,384]"
    );
    let (compiled, osr, property_stubs, _) = run(LOAD_SOURCE, JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr > 0, "load matrix must enter through loop OSR");
    assert!(
        property_stubs > 0,
        "load matrix must execute the JIT transition"
    );
}

#[test]
fn coercive_unary_ops_complete_in_place_with_gc_reentry() {
    let (oracle, _, _, _) = run(COERCION_SOURCE, JitSelection::InterpreterOnly);
    assert_eq!(
        oracle,
        "[30,6,384,256,4,3,2,\"coercion boom\",\"TypeError\",6]"
    );
    let (compiled, osr, _, reentrant_stubs) = run(COERCION_SOURCE, JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr > 0, "coercion matrix must enter through loop OSR");
    assert!(
        reentrant_stubs > 0,
        "coercive operands must execute the reentrant unary transition"
    );
}
