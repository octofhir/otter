//! Production-tier property-load and coercion miss completion coverage.
//!
//! # Contents
//! - Full `[[Get]]` completion for accessors, proxies, primitive receivers,
//!   megamorphic sites, exceptions, and allocating getter reentry.
//! - Exact compiled loads of boxed doubles, negative zero, NaN, and wide
//!   int32s through a cold decoder after GC-producing allocation churn.
//! - In-place `ToPrimitive`/`ToNumeric` completion through observable
//!   `@@toPrimitive` and `valueOf` hooks.
//! - Cold numeric-family completion for BigInt arithmetic/bitwise operations,
//!   uncommon Number conversions, and update-expression coercion.
//! - Generic method-resolution errors after observable proxy/accessor effects.
//!
//! # Invariants
//! - Every matrix runs through loop OSR in the production template tier.
//! - A throwing getter/coercion hook advances no later instruction and is
//!   never replayed after its observable counter increment.
//! - Interpreter and compiled completion values remain identical under GC,
//!   including every full-Value payload recovered from a compressed slot.

use otter_runtime::{JitSelection, Runtime, RuntimeExecutionStats, SourceInput};

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

const BOXED_PROPERTY_SOURCE: &str = r#"
const halfObject = { value: 0.5 };
const negativeZeroObject = { value: -0 };
const nanObject = { value: NaN };
const wideIntObject = { value: 2147483647 };

function churn(rounds) {
  let latest = "";
  for (let i = 0; i < rounds; i++) latest = "boxed-property-" + i;
  return latest.length;
}

function readBoxed(object, rounds) {
  let value;
  for (let i = 0; i < rounds; i++) value = object.value;
  return value;
}

let churnScore = churn(2048);
let half = readBoxed(halfObject, 512);
churnScore += churn(2048);
let negativeZero = readBoxed(negativeZeroObject, 512);
churnScore += churn(2048);
let nan = readBoxed(nanObject, 512);
churnScore += churn(2048);
let wideInt = readBoxed(wideIntObject, 512);

JSON.stringify([
  half,
  1 / negativeZero === -Infinity,
  nan !== nan,
  wideInt,
  churnScore > 0
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

const NUMERIC_SOURCE: &str = r#"
let expected = {
  sub: BigInt(7), mul: BigInt(18), div: BigInt(4), rem: BigInt(1),
  pow: BigInt(81), and: BigInt(0), or: BigInt(11), xor: BigInt(11),
  shl: BigInt(36), shr: BigInt(2), not: BigInt(-10)
};
function bigNumeric(a, b, e) {
  let score = 0;
  for (let i = 0; i < 4; i++) {
    if (a - b === e.sub) score++;
    if (a * b === e.mul) score++;
    if (a / b === e.div) score++;
    if (a % b === e.rem) score++;
    if (a ** b === e.pow) score++;
    if ((a & b) === e.and) score++;
    if ((a | b) === e.or) score++;
    if ((a ^ b) === e.xor) score++;
    if ((a << b) === e.shl) score++;
    if ((a >> b) === e.shr) score++;
    if (~a === e.not) score++;
  }
  return score;
}

let huge = 1e20;
let expectedHuge = huge | 0;
function uncommonNumbers(hugeValue, expectedValue) {
  let score = 0;
  for (let i = 0; i < 4; i++) {
    if ((NaN & 7) === 0) score++;
    if ((Infinity | 3) === 3) score++;
    if ((hugeValue ^ 0) === expectedValue) score++;
  }
  return score;
}

let incrementCalls = 0;
let incrementObject = {
  valueOf() {
    incrementCalls++;
    let values = [];
    for (let i = 0; i < 32; i++) values.push({i, text: "increment-" + i});
    return 4;
  }
};
function coerciveIncrement(object) {
  let sum = 0;
  for (let i = 0; i < 6; i++) {
    let value = object;
    sum += value++;
  }
  return sum;
}

let throwCalls = 0;
let throwProgress = 0;
let throwingIncrementObject = {
  valueOf() {
    throwCalls++;
    if (throwCalls === 3) throw new Error("increment boom");
    return 1;
  }
};
function throwingIncrement(object) {
  for (let i = 0; i < 6; i++) {
    let value = object;
    value++;
    throwProgress++;
  }
}

let throwName = "";
let bigScore = bigNumeric(BigInt(9), BigInt(2), expected);
let numberScore = uncommonNumbers(huge, expectedHuge);
let incrementSum = coerciveIncrement(incrementObject);
try { throwingIncrement(throwingIncrementObject); } catch (e) { throwName = e.message; }

JSON.stringify([
  bigScore, numberScore, incrementSum, incrementCalls,
  throwCalls, throwProgress, throwName
]);
"#;

const METHOD_ERROR_SOURCE: &str = r#"
let getCalls = 0;
let callCalls = 0;
let receiver = new Proxy({}, {
  get(target, key) {
    getCalls++;
    if (getCalls === 7) return 0;
    return function () { callCalls++; return 1; };
  }
});
function invokeUntilError(object) {
  let sum = 0;
  for (let i = 0; i < 12; i++) sum += object.method();
  return sum;
}
let errorName = "";
try { invokeUntilError(receiver); } catch (error) { errorName = error.name; }
JSON.stringify([getCalls, callCalls, errorName]);
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

fn run_boxed_properties(selection: JitSelection) -> (String, RuntimeExecutionStats) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(1)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(BOXED_PROPERTY_SOURCE.to_string()),
            "jit-boxed-property-load.js",
        )
        .expect("boxed property load matrix")
        .completion_string()
        .to_owned();
    (completion, runtime.execution_stats())
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
fn boxed_property_loads_recover_exact_values_through_cold_path_after_gc_churn() {
    let (oracle, _) = run_boxed_properties(JitSelection::InterpreterOnly);
    assert_eq!(oracle, "[0.5,true,true,2147483647,true]");

    let (compiled, stats) = run_boxed_properties(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(
        stats.jit_osr_attempts > 0,
        "fixture must request loop OSR: {stats:?}"
    );
    assert!(
        stats.jit_optimized_osr_entries > 0,
        "boxed property loop must enter compiled optimized code: {stats:?}"
    );
    assert!(
        stats.jit_runtime_property_stubs > 0,
        "the property IC must be populated through its runtime transition: {stats:?}"
    );
    assert!(
        stats.jit_runtime_property_stubs < 64,
        "warmed boxed property loads must stay compiled instead of missing per iteration: {stats:?}"
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

#[test]
fn numeric_family_misses_complete_in_place_without_replay() {
    let (oracle, _, _, _) = run(NUMERIC_SOURCE, JitSelection::InterpreterOnly);
    assert_eq!(oracle, "[44,12,24,6,3,2,\"increment boom\"]");
    let (compiled, osr, _, reentrant_stubs) = run(NUMERIC_SOURCE, JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr > 0, "numeric matrix must enter through loop OSR");
    assert!(
        reentrant_stubs > 0,
        "numeric misses must execute the shared reentrant transition"
    );
}

#[test]
fn method_resolution_error_does_not_replay_observable_get() {
    let (oracle, _, _, _) = run(METHOD_ERROR_SOURCE, JitSelection::InterpreterOnly);
    assert_eq!(oracle, "[7,6,\"TypeError\"]");
    let (compiled, osr, _, reentrant_stubs) = run(METHOD_ERROR_SOURCE, JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert!(osr > 0, "method matrix must enter through loop OSR");
    assert!(
        reentrant_stubs > 0,
        "proxy method resolution must execute the reentrant transition"
    );
}
