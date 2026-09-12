//! Machine IR direct-call execution coverage.
//!
//! # Contents
//! - Native publication proof for a monomorphic plain call.
//! - Exact return, callee-deopt, and throw semantics against the interpreter.
//! - Own/prototype guarded methods, exact receiver binding, and guard misses.
//! - Base constructors with `new.target`, receiver substitution, and accessors.
//! - Non-reentrant own-data prototype preparation and observable fallback.
//! - Generated receiver allocation, exact cold attribution, and page refill.
//! - Plain, base, derived, and superclass spread calls sharing that linkage.
//! - Nested generated calls retaining a tagged value across moving GC.
//!
//! # Invariants
//! - The fixture must execute through the Machine IR backend; semantic equality
//!   alone does not prove generated linkage.
//! - Generated linkage must be observed on the native hit path.
//! - An already-started callee is resumed after deopt and never replayed.
//! - Every throw restores the caller publication before later native reuse.
//! - Reentrant root records form a live nested chain until the deepest call
//!   returns, allowing the collector to rewrite allocator-owned value homes;
//!   post-call full GC observes an empty chain and the caller remains reusable.
//!
//! # See also
//! - `crates/otter-jit/src/machine/numeric` for call selection and emission.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";

const DIRECT_RETURN: &str = r#"
function target(value) {
  return value + 1;
}

function caller(fn, value) {
  return fn(value);
}

for (let i = 0; i < 5000; i++) {
  target(i);
  caller(target, i);
}

let checksum = 0;
for (let i = 0; i < 256; i++) {
  checksum += caller(target, i);
}
JSON.stringify([checksum, caller(target, 41)]);
"#;

const DIRECT_OVERFLOW: &str = r#"
function target(value) {
  return value + 1;
}

function caller(fn, value) {
  return fn(value);
}

for (let i = 0; i < 5000; i++) {
  target(i);
  caller(target, i);
}

JSON.stringify([caller(target, 2147483647), caller(target, 41)]);
"#;

const DIRECT_THROW: &str = r#"
function target(value) {
  if (value < 0) throw "boom";
  return value + 1;
}

function caller(fn, value) {
  return fn(value);
}

for (let i = 0; i < 5000; i++) {
  target(i);
  caller(target, i);
}

let caught = "missing";
try {
  caller(target, -1);
} catch (error) {
  caught = error;
}
const recovered = caller(target, 41);
JSON.stringify([caught, recovered]);
"#;

const DIRECT_DEOPT_THEN_LOCAL_CATCH: &str = r#"
let effects = 0;
let catches = 0;

function target(value) {
  try {
    effects++;
    const widened = value + 1;
    if (value === 2147483647) throw "after-stack-deopt";
    return widened;
  } catch (error) {
    catches++;
    return error;
  }
}

function caller(fn, value) {
  return fn(value);
}

for (let i = 0; i < 5000; i++) {
  target(i);
  caller(target, i);
}

const effectsBefore = effects;
const catchesBefore = catches;
JSON.stringify([
  caller(target, 2147483647),
  effects - effectsBefore,
  catches - catchesBefore,
  caller(target, 41)
]);
"#;

const EVAL_ENV_DIRECT_CALL: &str = r#"
function evalEnvFactory() {
  const target = function evalEnvTarget(delta) {
    return dynamicBinding + delta;
  };
  eval("var dynamicBinding = 40;");
  return target;
}

globalThis.evalEnvTarget = evalEnvFactory();
globalThis.evalEnvCaller = function evalEnvCaller(fn, delta) {
  return fn(delta);
};

for (let i = 0; i < 5000; i++) {
  evalEnvTarget(i & 1);
  evalEnvCaller(evalEnvTarget, i & 1);
}
JSON.stringify([evalEnvCaller(evalEnvTarget, 2), evalEnvTarget(3)]);
"#;

const NESTED_GC_SETUP: &str = r#"
function allocator(count) {
  let checksum = 0;
  for (let i = 0; i < count; i++) {
    const item = { value: i, padding: "allocation-padding-" + i };
    globalThis.__machineGcSink.push(item);
    checksum += item.value & 1;
  }
  return checksum;
}

function middle(fn, count) {
  return fn(count);
}

function outer(next, fn, marker, count) {
  const value = next(fn, count);
  return marker + value;
}

globalThis.__machineGcSink = [];
for (let i = 0; i < 5000; i++) {
  allocator(0);
  middle(allocator, 0);
  outer(middle, allocator, "warm:" + i, 0);
}
"#;

const NESTED_GC_PROBE: &str = r#"
const marker = "kept:" + 17;
outer(middle, allocator, marker, 200000);
"#;

const OWN_METHOD: &str = r#"
function ownMethod(delta) {
  return this.base + delta;
}

const ownReceiver = { base: 40, method: ownMethod };
function ownCaller(receiver, delta) {
  return receiver.method(delta);
}

for (let i = 0; i < 5000; i++) ownCaller(ownReceiver, i);
JSON.stringify([ownCaller(ownReceiver, 2), ownReceiver.base]);
"#;

const PROTOTYPE_METHOD: &str = r#"
function prototypeMethod(delta) {
  return this.base + delta;
}

const methodPrototype = { method: prototypeMethod };
const prototypeReceiver = Object.create(methodPrototype);
prototypeReceiver.base = 40;
function prototypeCaller(receiver, delta) {
  return receiver.method(delta);
}

for (let i = 0; i < 5000; i++) prototypeCaller(prototypeReceiver, i);
JSON.stringify([prototypeCaller(prototypeReceiver, 2), prototypeReceiver.base]);
"#;

const METHOD_COLD_EXITS: &str = r#"
let accessorEffects = 0;
function guardedMethod(value) {
  if (value < 0) throw "method-boom";
  return value + 1;
}

const stableReceiver = { method: guardedMethod };
const unstableReceiver = {
  get method() {
    accessorEffects++;
    return guardedMethod;
  }
};
function guardedCaller(receiver, value) {
  return receiver.method(value);
}

for (let i = 0; i < 5000; i++) guardedCaller(stableReceiver, i);
const overflow = guardedCaller(stableReceiver, 2147483647);
let caught = "missing";
try {
  guardedCaller(stableReceiver, -1);
} catch (error) {
  caught = error;
}
const miss = guardedCaller(unstableReceiver, 4);
const reused = guardedCaller(stableReceiver, 41);
JSON.stringify([overflow, caught, miss, accessorEffects, reused]);
"#;

const METHOD_SPILLS: &str = r#"
function manyArgumentMethod(a, b, c, d, e, f, g, h, i, j, k, l, m, n, o) {
  return this.marker + a + o;
}

const spillReceiver = { marker: "spill:", method: manyArgumentMethod };
function spillCaller(receiver, a, b, c, d, e, f, g, h, i, j, k, l, m, n, o) {
  return receiver.method(a, b, c, d, e, f, g, h, i, j, k, l, m, n, o);
}

for (let i = 0; i < 5000; i++) {
  spillCaller(spillReceiver, i, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
}
spillCaller(spillReceiver, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
"#;

const METHOD_LANDING_PAD: &str = r#"
let landingEffects = 0;
let landingThrown = undefined;
function landingMethod(value) {
  landingEffects++;
  if (value < 0) {
    const thrown = { kind: "landing-boom", value };
    landingThrown = thrown;
    throw thrown;
  }
  return value + 1;
}

const landingReceiver = { method: landingMethod };
function landingCaller(receiver, value) {
  let result = undefined;
  try {
    result = receiver.method(value);
  } catch (error) {
    result = [error === landingThrown, error.kind, error.value];
  }
  return result;
}

for (let i = 0; i < 5000; i++) landingCaller(landingReceiver, i);
const landingEffectsBefore = landingEffects;
JSON.stringify([
  landingCaller(landingReceiver, 7),
  landingCaller(landingReceiver, -5),
  landingCaller(landingReceiver, 41),
  landingEffects - landingEffectsBefore
]);
"#;

const METHOD_GC_SETUP: &str = r#"
function allocatingMethod(count) {
  let checksum = 0;
  for (let i = 0; i < count; i++) {
    const item = { value: i, padding: "method-allocation-padding-" + i };
    globalThis.__machineMethodGcSink.push(item);
    checksum += item.value & 1;
  }
  return this.marker + checksum;
}

const methodGcReceiver = { marker: "method-root:", method: allocatingMethod };
function methodGcCaller(receiver, count) {
  return receiver.method(count);
}

globalThis.__machineMethodGcSink = [];
for (let i = 0; i < 5000; i++) methodGcCaller(methodGcReceiver, 0);
"#;

const METHOD_GC_PROBE: &str = r#"
methodGcCaller(methodGcReceiver, 200000);
"#;

const RECURSIVE_CALLS: &str = r#"
function recursive(self, value) {
  if (value <= 0) return value;
  return self(self, value - 1);
}

function mutualLeft(other, self, value) {
  if (value <= 0) return 1;
  return other(self, other, value - 1);
}

function mutualRight(other, self, value) {
  if (value <= 0) return 2;
  return other(self, other, value - 1);
}

for (let i = 0; i < 5000; i++) {
  recursive(recursive, 4);
  mutualLeft(mutualRight, mutualLeft, 4);
  mutualRight(mutualLeft, mutualRight, 4);
}

JSON.stringify([
  recursive(recursive, 64),
  mutualLeft(mutualRight, mutualLeft, 64),
  mutualLeft(mutualRight, mutualLeft, 63)
]);
"#;

const BASE_CONSTRUCT: &str = r#"
let prototypeGets = 0;
const instancePrototype = { marker: "proto" };

function Base(value) {
  this.value = value;
  this.targetIsBase = new.target === Base;
  return 17;
}

Object.defineProperty(Base, "prototype", {
  configurable: true,
  get() {
    prototypeGets++;
    return instancePrototype;
  }
});

function construct(Ctor, value) {
  return new Ctor(value);
}

for (let i = 0; i < 5000; i++) construct(Base, i);
const result = construct(Base, 42);
JSON.stringify([
  result.value,
  result.targetIsBase,
  Object.getPrototypeOf(result) === instancePrototype,
  prototypeGets
]);
"#;

const DEFAULT_BASE_CONSTRUCT: &str = r#"
function DefaultBase(value) {
  this.value = value;
  this.targetIsBase = new.target === DefaultBase;
}

function constructDefault(Ctor, value) {
  return new Ctor(value);
}

for (let i = 0; i < 5000; i++) constructDefault(DefaultBase, i);
const result = constructDefault(DefaultBase, 42);
JSON.stringify([
  result.value,
  result.targetIsBase,
  Object.getPrototypeOf(result) === DefaultBase.prototype
]);
"#;

const SIMPLE_SHAPED_CONSTRUCT_FAMILY: &str = r#"
function ShapedBase(left) {
  this.left = left;
  this.right = 7;
}

class ShapedDerived extends ShapedBase {
  constructor(left) {
    super(left);
  }
}

function constructShaped(Ctor, left) {
  return new Ctor(left);
}

function constructShapedSpread(Ctor, args) {
  return new Ctor(...args);
}

function constructShapedDerived(Ctor, left) {
  return new Ctor(left);
}

for (let i = 0; i < 5000; i++) new ShapedDerived(i);

for (let i = 0; i < 5000; i++) {
  constructShaped(ShapedBase, i);
  constructShapedSpread(ShapedBase, [i]);
  constructShapedDerived(ShapedDerived, i);
}

const fixed = constructShaped(ShapedBase, 35);
const spread = constructShapedSpread(ShapedBase, [35]);
const derived = constructShapedDerived(ShapedDerived, 35);
JSON.stringify([
  fixed.left + fixed.right,
  spread.left + spread.right,
  derived.left + derived.right,
  Object.keys(fixed).join(","),
  Object.getPrototypeOf(derived) === ShapedDerived.prototype
]);
"#;

const GENERATED_RECEIVER_ALLOCATION: &str = r#"
class AllocationBase {
  constructor(value) {
    this.base = value + 1;
  }
}

class AllocationDerived extends AllocationBase {
  constructor(value) {
    super(value);
    this.derived = value + 2;
  }
}

function allocateFixed(Ctor, count) {
  let checksum = 0;
  for (let i = 0; i < count; i++) checksum += new Ctor(i).base;
  return checksum;
}

function allocateSpread(Ctor, count) {
  let checksum = 0;
  for (let i = 0; i < count; i++) checksum += new Ctor(...[i]).base;
  return checksum;
}

function allocateDerived(Ctor, count) {
  let checksum = 0;
  for (let i = 0; i < count; i++) {
    const value = new Ctor(i);
    checksum += value.base + value.derived;
  }
  return checksum;
}

new AllocationBase(-1);
new AllocationDerived(-1);
allocateFixed(AllocationBase, 5000);
allocateSpread(AllocationBase, 5000);
allocateDerived(AllocationDerived, 5000);

allocateFixed(AllocationBase, 20000)
  + allocateSpread(AllocationBase, 20000)
  + allocateDerived(AllocationDerived, 20000);
"#;

const SIMPLE_SHAPE_OBSERVABLE_SETTER: &str = r#"
let setterEffects = 0;
const setterPrototype = {
  set value(next) {
    setterEffects++;
    this.observed = next;
  }
};

function SetterBase(value) {
  this.value = value;
}
SetterBase.prototype = setterPrototype;

function constructSetter(Ctor, value) {
  return new Ctor(value);
}

for (let i = 0; i < 5000; i++) constructSetter(SetterBase, i);
const result = constructSetter(SetterBase, 42);
JSON.stringify([
  setterEffects,
  Object.hasOwn(result, "value"),
  result.observed,
  Object.getPrototypeOf(result) === setterPrototype
]);
"#;

const CONSTRUCT_COLD_EXITS: &str = r#"
let basePrototypeGets = 0;
let otherPrototypeGets = 0;
const basePrototype = { kind: "base" };
const otherPrototype = { kind: "other" };

function Base(value) {
  if (value.fail) throw "construct-boom";
  return value;
}

function Other(value) {
  this.value = value;
}

Object.defineProperty(Base, "prototype", {
  configurable: true,
  get() {
    basePrototypeGets++;
    return basePrototype;
  }
});
Object.defineProperty(Other, "prototype", {
  configurable: true,
  get() {
    otherPrototypeGets++;
    return otherPrototype;
  }
});

function construct(Ctor, value) {
  return new Ctor(value);
}

const warmOverride = { override: 1, fail: false };
for (let i = 0; i < 5000; i++) construct(Base, warmOverride);
const override = construct(Base, { override: 42, fail: false });
let caught = "missing";
try {
  construct(Base, { fail: true });
} catch (error) {
  caught = error;
}
const miss = construct(Other, 7);
const recovered = construct(Base, { override: 10, fail: false });
JSON.stringify([
  override.override,
  caught,
  miss.value,
  Object.getPrototypeOf(miss) === otherPrototype,
  recovered.override,
  basePrototypeGets,
  otherPrototypeGets
]);
"#;

const CONSTRUCT_GC: &str = r#"
let constructGcProbe = false;
const constructGcPrototype = { marker: "prototype" };
globalThis.__machineConstructGcSink = [];

function GcBase(marker) {
  this.marker = marker;
}

Object.defineProperty(GcBase, "prototype", {
  configurable: true,
  get() {
    if (constructGcProbe) {
      for (let i = 0; i < 200000; i++) {
        globalThis.__machineConstructGcSink.push({ i, padding: "construct-gc-" + i });
      }
    }
    return constructGcPrototype;
  }
});

function constructGc(Ctor, marker) {
  return new Ctor(marker);
}

for (let i = 0; i < 5000; i++) constructGc(GcBase, "warm:" + i);
constructGcProbe = true;
const marker = "kept:" + 42;
const result = constructGc(GcBase, marker);
JSON.stringify([
  result.marker,
  Object.getPrototypeOf(result) === constructGcPrototype,
  globalThis.__machineConstructGcSink.length
]);
"#;

const DERIVED_CONSTRUCT: &str = r#"
let baseRuns = 0;

class Base {
  constructor(value) {
    baseRuns++;
    this.value = value;
  }
}

class Derived extends Base {
  constructor(value) {
    super(value);
    this.extra = value + 1;
  }
}

class Returner extends Base {
  constructor(value) {
    return value;
  }
}

// Promote the derived body before generated callers begin selecting it. The
// generated-entry counters are reconciled at the outer activation boundary,
// independently of the constructor-linkage contract exercised below.
for (let i = 0; i < 5000; i++) {
  new Derived(i);
}
baseRuns = 0;

function constructDerived(Ctor, value) {
  return new Ctor(value);
}

function constructReturner(Ctor, value) {
  return new Ctor(value);
}

for (let i = 0; i < 5000; i++) {
  constructDerived(Derived, i);
  constructReturner(Returner, { warm: i });
}

const result = constructDerived(Derived, 42);
const override = { marker: "override" };
const returned = constructReturner(Returner, override);
JSON.stringify([
  result.value,
  result.extra,
  Object.getPrototypeOf(result) === Derived.prototype,
  returned === override,
  baseRuns
]);
"#;

const SPREAD_CALL_FAMILY: &str = r#"
function spreadTarget(a, b) {
  if (a === -7) throw "spread-boom";
  return a + b;
}

function callSpread(fn, args, tail) {
  const value = fn(...args);
  return value + tail;
}

function SpreadBase(a, b) {
  if (a === -7) throw "construct-spread-boom";
  this.total = a + b;
  this.targetMatches = new.target === SpreadBase;
  if (a && a.override) return a;
}

function constructSpreadBase(Ctor, args) {
  return new Ctor(...args);
}

function constructSpreadDerived(Ctor, args) {
  return new Ctor(...args);
}

class SpreadSuper {
  constructor(a, b) {
    this.total = a * 3 + b;
  }
}

class SpreadDerived extends SpreadSuper {
  constructor(args) {
    super(...args);
  }
}

const callArgs = [1, 2];
const baseArgs = [1, 2];
const derivedArgs = [[1, 2]];
for (let i = 0; i < 5000; i++) {
  spreadTarget(i, 2);
  new SpreadBase(i, 2);
  new SpreadDerived([i, 2]);
  callArgs[0] = i;
  baseArgs[0] = i;
  derivedArgs[0][0] = i;
  callSpread(spreadTarget, callArgs, 1);
  constructSpreadBase(SpreadBase, baseArgs);
  constructSpreadDerived(SpreadDerived, derivedArgs);
}

callArgs[0] = 2147483647;
callArgs[1] = 1;
const overflow = callSpread(spreadTarget, callArgs, 0);
callArgs[0] = -7;
let callThrow = "missing";
try { callSpread(spreadTarget, callArgs, 0); } catch (error) { callThrow = error; }

baseArgs[0] = 40;
baseArgs[1] = 2;
const base = constructSpreadBase(SpreadBase, baseArgs);
const override = { override: true, marker: "spread-override" };
const returned = constructSpreadBase(SpreadBase, [override, 9]);
let constructThrow = "missing";
try { constructSpreadBase(SpreadBase, [-7, 1]); } catch (error) { constructThrow = error; }

derivedArgs[0][0] = 13;
derivedArgs[0][1] = 2;
const derived = constructSpreadDerived(SpreadDerived, derivedArgs);
callArgs[0] = 40;
callArgs[1] = 2;
const reused = callSpread(spreadTarget, callArgs, 0);
JSON.stringify([
  overflow,
  callThrow,
  base.total,
  base.targetMatches,
  returned === override,
  constructThrow,
  derived.total,
  reused
]);
"#;

const SPREAD_CONSTRUCT_GC: &str = r#"
let spreadGcProbe = false;
const spreadGcPrototype = { marker: "spread-gc-prototype" };
globalThis.__spreadGcSink = [];

function SpreadGcBase(marker) {
  this.marker = marker;
}

Object.defineProperty(SpreadGcBase, "prototype", {
  configurable: true,
  get() {
    if (spreadGcProbe) {
      for (let i = 0; i < 200000; i++) {
        globalThis.__spreadGcSink.push({ i, padding: "spread-gc-" + i });
      }
    }
    return spreadGcPrototype;
  }
});

function constructSpreadGc(Ctor, args) {
  return new Ctor(...args);
}

const warmArgs = ["warm"];
for (let i = 0; i < 5000; i++) constructSpreadGc(SpreadGcBase, warmArgs);
spreadGcProbe = true;
const liveArgs = ["kept:42"];
const result = constructSpreadGc(SpreadGcBase, liveArgs);
JSON.stringify([
  result.marker,
  Object.getPrototypeOf(result) === spreadGcPrototype,
  globalThis.__spreadGcSink.length
]);
"#;

struct RunResult {
    completion: String,
    stats: RuntimeExecutionStats,
    used_machine_direct_call: bool,
    used_machine_method_call: bool,
    used_machine_method_landing_ack: bool,
    used_machine_construct: bool,
    used_generated_construct: bool,
    used_fast_construct_prepare: bool,
    used_observable_construct_prepare: bool,
    used_generated_receiver_allocation: bool,
    used_cold_receiver_allocation: bool,
    used_machine_derived_construct: bool,
    used_machine_super_construct: bool,
    used_generated_super_construct: bool,
    used_machine_spread_arguments: bool,
    used_machine_constructor_field: bool,
    used_machine_class_super_load: bool,
    used_machine_derived_this_bind: bool,
    used_direct_construct_result_fast: bool,
    used_direct_construct_result_throw: bool,
    compile_diagnostics: Vec<String>,
}

fn run(source: &'static str, name: &'static str, selection: JitSelection) -> RunResult {
    let artifacts = matches!(selection, JitSelection::ProductionTiered);
    let builder = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX);
    let mut runtime = if artifacts {
        builder
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .build()
    } else {
        builder.build()
    }
    .expect("Machine direct-call runtime");
    let result = runtime
        .run_script(SourceInput::from_javascript(source), name)
        .unwrap_or_else(|error| {
            panic!("Machine direct-call fixture {name} ({selection:?}): {error:?}")
        });
    let artifact_has = |needle: &str, machine_only: bool| {
        result.jit_artifacts().is_some_and(|batch| {
            batch.bundles().iter().any(|bundle| {
                (!machine_only
                    || bundle
                        .file(JitArtifactFileName::OptimizedIr)
                        .is_some_and(|file| {
                            file.contents()
                                .starts_with(b"; backend=otter-machine-ir scalar-function\n")
                        }))
                    && bundle
                        .file(JitArtifactFileName::Relocations)
                        .is_some_and(|file| {
                            std::str::from_utf8(file.contents())
                                .is_ok_and(|text| text.contains(needle))
                        })
            })
        })
    };
    let code_map_has = |needle: &str| {
        result.jit_artifacts().is_some_and(|batch| {
            batch.bundles().iter().any(|bundle| {
                bundle
                    .file(JitArtifactFileName::CodeMap)
                    .is_some_and(|file| {
                        std::str::from_utf8(file.contents()).is_ok_and(|text| text.contains(needle))
                    })
            })
        })
    };
    let used_machine_direct_call = artifact_has("directCallEntryCell", true);
    let used_machine_method_call = artifact_has("\"callKind\": \"method\"", true)
        || artifact_has("\"callKind\":\"method\"", true);
    let used_machine_method_landing_ack = result.jit_artifacts().is_some_and(|batch| {
        batch.bundles().iter().any(|bundle| {
            bundle.manifest().function_name() == "landingCaller"
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
                && bundle
                    .file(JitArtifactFileName::Relocations)
                    .is_some_and(|file| {
                        std::str::from_utf8(file.contents()).is_ok_and(|text| {
                            text.contains("jit_acknowledge_caught_throw")
                                && (text.contains("\"callKind\": \"method\"")
                                    || text.contains("\"callKind\":\"method\""))
                        })
                    })
                && bundle
                    .file(JitArtifactFileName::CodeMap)
                    .is_some_and(|file| {
                        std::str::from_utf8(file.contents())
                            .is_ok_and(|text| text.contains("machineDirectMethodCandidate"))
                    })
        })
    });
    let used_machine_construct = artifact_has("\"callKind\": \"construct\"", true)
        || artifact_has("\"callKind\":\"construct\"", true);
    let used_generated_construct = artifact_has("\"callKind\": \"construct\"", false)
        || artifact_has("\"callKind\":\"construct\"", false);
    let used_fast_construct_prepare = code_map_has("directConstructPrepareFast");
    let used_observable_construct_prepare = code_map_has("directConstructPrepareObservable");
    let used_generated_receiver_allocation = code_map_has("directConstructReceiverAllocFast");
    let used_cold_receiver_allocation = code_map_has("directConstructReceiverAllocCold");
    let used_machine_derived_construct = artifact_has("\"callKind\": \"derivedConstruct\"", true)
        || artifact_has("\"callKind\":\"derivedConstruct\"", true);
    let used_machine_super_construct = artifact_has("\"callKind\": \"superConstruct\"", true)
        || artifact_has("\"callKind\":\"superConstruct\"", true);
    let used_generated_super_construct = artifact_has("\"callKind\": \"superConstruct\"", false)
        || artifact_has("\"callKind\":\"superConstruct\"", false);
    let used_machine_spread_arguments = artifact_has("\"argumentMode\": \"spread\"", false)
        || artifact_has("\"argumentMode\":\"spread\"", false);
    let used_machine_constructor_field = code_map_has("machineConstructorFieldTransition");
    let used_machine_class_super_load = code_map_has("machineClassSuperLoad");
    let used_machine_derived_this_bind =
        code_map_has("machineDerivedThisBindFast") && code_map_has("machineDerivedThisBindCold");
    let used_direct_construct_result_fast = code_map_has("directConstructResultFast");
    let used_direct_construct_result_throw = code_map_has("directConstructResultThrow");
    let compile_diagnostics = result
        .jit_debug_report()
        .map(|report| {
            report
                .events()
                .iter()
                .map(|event| format!("{event:?}"))
                .collect()
        })
        .unwrap_or_default();
    RunResult {
        completion: result.completion_string().to_owned(),
        stats: runtime.execution_stats(),
        used_machine_direct_call,
        used_machine_method_call,
        used_machine_method_landing_ack,
        used_machine_construct,
        used_generated_construct,
        used_fast_construct_prepare,
        used_observable_construct_prepare,
        used_generated_receiver_allocation,
        used_cold_receiver_allocation,
        used_machine_derived_construct,
        used_machine_super_construct,
        used_generated_super_construct,
        used_machine_spread_arguments,
        used_machine_constructor_field,
        used_machine_class_super_load,
        used_machine_derived_this_bind,
        used_direct_construct_result_fast,
        used_direct_construct_result_throw,
        compile_diagnostics,
    }
}

fn assert_generated_spread_call(result: &RunResult) {
    assert!(result.stats.jit_generated_calls > 0);
    assert!(
        result.used_machine_spread_arguments,
        "fixture must publish spread argument materialization on shared direct linkage"
    );
}

fn assert_machine_construct(result: &RunResult) {
    assert_machine_direct_call(result);
    assert!(
        result.used_machine_construct,
        "fixture must publish a typed Machine IR construct target"
    );
    assert!(result.used_fast_construct_prepare);
    assert!(result.used_observable_construct_prepare);
}

fn assert_machine_derived_construct(result: &RunResult) {
    assert_machine_direct_call(result);
    assert!(
        result.used_machine_derived_construct,
        "fixture must publish a typed Machine IR derived construct target"
    );
    assert!(
        result.used_machine_super_construct,
        "fixture must publish a typed Machine IR super construct target; diagnostics={:?}",
        result.compile_diagnostics
    );
    assert!(result.used_machine_constructor_field);
    assert!(result.used_machine_class_super_load);
    assert!(result.used_machine_derived_this_bind);
    assert!(result.used_direct_construct_result_fast);
    assert!(result.used_direct_construct_result_throw);
}

fn assert_machine_method_call(result: &RunResult) {
    assert_machine_direct_call(result);
    assert!(
        result.used_machine_method_call,
        "fixture must publish a typed Machine IR method-call target"
    );
}

fn assert_machine_direct_call(result: &RunResult) {
    assert!(
        result.stats.jit_generated_calls > 0,
        "fixture must enter a generated callee; diagnostics={:?}",
        result.compile_diagnostics
    );
    assert!(
        result.used_machine_direct_call,
        "fixture must publish a Machine IR body containing direct linkage: {:?}",
        result.compile_diagnostics
    );
}

#[test]
fn direct_return_executes_through_machine_ir() {
    let oracle = run(
        DIRECT_RETURN,
        "jit-machine-direct-return.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        DIRECT_RETURN,
        "jit-machine-direct-return.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[32896,42]");
    assert_machine_direct_call(&compiled);
}

#[test]
fn base_construct_executes_through_machine_ir() {
    let oracle = run(
        BASE_CONSTRUCT,
        "jit-machine-base-construct.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        BASE_CONSTRUCT,
        "jit-machine-base-construct.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[42,true,true,5001]");
    assert_machine_construct(&compiled);
    assert!(compiled.stats.jit_reentrant_stub_transitions > 0);
}

#[test]
fn default_base_construct_uses_non_reentrant_receiver_preparation_in_both_tiers() {
    let oracle = run(
        DEFAULT_BASE_CONSTRUCT,
        "jit-default-base-construct.js",
        JitSelection::InterpreterOnly,
    );
    let template = run(
        DEFAULT_BASE_CONSTRUCT,
        "jit-default-base-construct.js",
        JitSelection::Template,
    );
    let production = run(
        DEFAULT_BASE_CONSTRUCT,
        "jit-default-base-construct.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(oracle.completion, "[42,true,true]");
    assert_eq!(template.completion, oracle.completion);
    assert_eq!(production.completion, oracle.completion);
    assert!(template.stats.jit_generated_calls > 0);
    assert!(template.stats.jit_alloc_stub_transitions > 0);
    assert_machine_construct(&production);
    assert!(production.stats.jit_alloc_stub_transitions > 0);
}

#[test]
fn simple_constructor_shapes_cover_fixed_spread_and_super_linkage() {
    let oracle = run(
        SIMPLE_SHAPED_CONSTRUCT_FAMILY,
        "jit-simple-shaped-construct-family.js",
        JitSelection::InterpreterOnly,
    );
    let template = run(
        SIMPLE_SHAPED_CONSTRUCT_FAMILY,
        "jit-simple-shaped-construct-family.js",
        JitSelection::Template,
    );
    let production = run(
        SIMPLE_SHAPED_CONSTRUCT_FAMILY,
        "jit-simple-shaped-construct-family.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(oracle.completion, r#"[42,42,42,"left,right",true]"#);
    assert_eq!(template.completion, oracle.completion);
    assert_eq!(production.completion, oracle.completion);
    assert!(template.stats.jit_generated_calls > 0);
    assert!(template.stats.property_store_misses < 500);
    assert!(template.stats.jit_alloc_stub_transitions > 0);
    assert_generated_spread_call(&production);
    assert!(production.used_generated_construct);
    assert!(
        production.used_machine_super_construct || production.used_generated_super_construct,
        "simple derived constructor must use typed super linkage; diagnostics={:?}",
        production.compile_diagnostics
    );
    assert!(production.used_fast_construct_prepare);
    assert!(production.stats.property_store_misses < 500);
    assert!(production.used_generated_receiver_allocation);
    assert!(production.used_cold_receiver_allocation);
    if std::env::var_os("OTTER_GC_STRESS").is_some() {
        assert_eq!(production.stats.jit_receiver_alloc_generated, 0);
        assert!(production.stats.jit_receiver_alloc_space_misses > 0);
        assert!(production.stats.jit_receiver_alloc_gc_transitions > 0);
    } else {
        assert!(production.stats.jit_receiver_alloc_generated > 0);
    }
    assert_eq!(
        production.stats.jit_receiver_alloc_attempts,
        production.stats.jit_receiver_alloc_generated
            + production.stats.jit_receiver_alloc_guard_misses
            + production.stats.jit_receiver_alloc_space_misses
    );
    assert_eq!(
        production.stats.jit_receiver_alloc_cold_transitions,
        production.stats.jit_receiver_alloc_guard_misses
            + production.stats.jit_receiver_alloc_space_misses
    );
    assert_eq!(
        production.stats.jit_receiver_alloc_rust_transitions,
        production.stats.jit_receiver_alloc_cold_transitions
    );
    assert_eq!(production.stats.jit_receiver_alloc_deopts, 0);
    assert_eq!(production.stats.jit_receiver_alloc_oom, 0);
}

#[test]
fn generated_receiver_allocation_owns_super_hot_path_and_refills() {
    let production = run(
        GENERATED_RECEIVER_ALLOCATION,
        "jit-generated-receiver-allocation.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(production.completion, "800060000");
    assert!(production.stats.jit_generated_calls > 0);
    assert!(
        production.stats.jit_receiver_alloc_generated > 20_000,
        "stats={:?}",
        production.stats
    );
    assert!(production.stats.jit_receiver_alloc_space_misses > 0);
    assert_eq!(production.stats.jit_receiver_alloc_guard_misses, 0);
    assert_eq!(
        production.stats.jit_receiver_alloc_attempts,
        production.stats.jit_receiver_alloc_generated
            + production.stats.jit_receiver_alloc_space_misses
    );
    assert_eq!(
        production.stats.jit_receiver_alloc_cold_transitions,
        production.stats.jit_receiver_alloc_space_misses
    );
    assert_eq!(
        production.stats.jit_receiver_alloc_rust_transitions,
        production.stats.jit_receiver_alloc_cold_transitions
    );
    assert_eq!(
        production.stats.jit_receiver_alloc_refills,
        production.stats.jit_receiver_alloc_space_misses
    );
    assert_eq!(production.stats.jit_receiver_alloc_deopts, 0);
    assert_eq!(production.stats.jit_receiver_alloc_oom, 0);
    assert!(
        production.stats.jit_alloc_stub_transitions
            < production.stats.jit_receiver_alloc_attempts / 100
    );
}

#[test]
fn inherited_setter_prevents_constructor_preshape_without_losing_effects() {
    let oracle = run(
        SIMPLE_SHAPE_OBSERVABLE_SETTER,
        "jit-simple-shape-setter.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        SIMPLE_SHAPE_OBSERVABLE_SETTER,
        "jit-simple-shape-setter.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(oracle.completion, r#"[5001,false,42,true]"#);
    assert_eq!(compiled.completion, oracle.completion);
    assert!(compiled.stats.jit_generated_calls > 0);
    assert!(compiled.stats.jit_runtime_property_stubs > 0);
}

#[test]
fn construct_object_throw_and_guard_miss_are_not_replayed() {
    let oracle = run(
        CONSTRUCT_COLD_EXITS,
        "jit-machine-construct-cold-exits.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        CONSTRUCT_COLD_EXITS,
        "jit-machine-construct-cold-exits.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(
        compiled.completion,
        r#"[42,"construct-boom",7,true,10,5003,1]"#
    );
    assert_machine_construct(&compiled);
}

#[test]
fn construct_receiver_and_arguments_survive_reentrant_moving_gc() {
    let compiled = run(
        CONSTRUCT_GC,
        "jit-machine-construct-gc.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, r#"["kept:42",true,200000]"#);
    assert_machine_construct(&compiled);
    assert!(
        compiled.stats.gc_minor_cycles > 0,
        "prototype getter must trigger moving GC while construct roots are published"
    );
}

#[test]
fn derived_and_super_construct_execute_through_machine_ir() {
    let oracle = run(
        DERIVED_CONSTRUCT,
        "jit-machine-derived-construct.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        DERIVED_CONSTRUCT,
        "jit-machine-derived-construct.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[42,43,true,true,5001]");
    assert_machine_derived_construct(&compiled);
}

#[test]
fn complete_spread_call_family_uses_shared_generated_linkage() {
    let oracle = run(
        SPREAD_CALL_FAMILY,
        "jit-machine-spread-call-family.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        SPREAD_CALL_FAMILY,
        "jit-machine-spread-call-family.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(
        compiled.completion,
        r#"[2147483648,"spread-boom",42,true,true,"construct-spread-boom",41,42]"#
    );
    assert_generated_spread_call(&compiled);
    assert!(compiled.used_generated_construct);
    assert!(compiled.used_generated_super_construct);
    assert!(compiled.stats.jit_generated_call_deopts > 0);
}

#[test]
fn spread_array_survives_receiver_preparation_moving_gc() {
    let compiled = run(
        SPREAD_CONSTRUCT_GC,
        "jit-machine-spread-construct-gc.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, r#"["kept:42",true,200000]"#);
    assert_generated_spread_call(&compiled);
    assert!(compiled.used_generated_construct);
    assert!(compiled.stats.gc_minor_cycles > 0);
}

#[test]
fn callee_overflow_deopt_resumes_without_replaying_call() {
    let oracle = run(
        DIRECT_OVERFLOW,
        "jit-machine-direct-overflow.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        DIRECT_OVERFLOW,
        "jit-machine-direct-overflow.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[2147483648,42]");
    assert_machine_direct_call(&compiled);
    assert!(
        compiled.stats.jit_generated_call_deopts > 0,
        "overflow must resume the already-started generated callee"
    );
}

#[test]
fn callee_throw_restores_publication_and_caller_is_reusable() {
    let oracle = run(
        DIRECT_THROW,
        "jit-machine-direct-throw.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        DIRECT_THROW,
        "jit-machine-direct-throw.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, r#"["boom",42]"#);
    assert_machine_direct_call(&compiled);
    assert!(
        compiled.stats.jit_generated_call_deopts > 0,
        "unsupported throw opcode must resume the already-started generated callee"
    );
}

#[test]
fn stack_callee_deopt_rebuilds_local_catch_without_replaying_effects() {
    let oracle = run(
        DIRECT_DEOPT_THEN_LOCAL_CATCH,
        "jit-machine-direct-deopt-local-catch.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        DIRECT_DEOPT_THEN_LOCAL_CATCH,
        "jit-machine-direct-deopt-local-catch.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, r#"["after-stack-deopt",1,1,42]"#);
    assert_machine_direct_call(&compiled);
    assert!(
        compiled.stats.jit_generated_call_deopts > 0,
        "the stack-owned callee must deopt before the local throw"
    );
}

#[test]
fn generated_call_carries_closure_eval_env_after_factory_frame_and_full_gc() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_osr_threshold(u32::MAX)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("eval-env direct-call runtime");
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(EVAL_ENV_DIRECT_CALL),
            "jit-machine-eval-env-direct-call.js",
        )
        .expect("eval-env direct-call setup");
    assert_eq!(setup.completion_string(), "[42,43]");
    let used_machine_direct_call = setup.jit_artifacts().is_some_and(|batch| {
        batch.bundles().iter().any(|bundle| {
            bundle
                .file(JitArtifactFileName::OptimizedIr)
                .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
                && bundle
                    .file(JitArtifactFileName::Relocations)
                    .is_some_and(|file| {
                        std::str::from_utf8(file.contents())
                            .is_ok_and(|text| text.contains("directCallEntryCell"))
                    })
        })
    });
    assert!(
        used_machine_direct_call,
        "evalEnvCaller must use generated linkage for the non-null env closure"
    );

    runtime.force_gc().expect("full GC over captured eval env");
    let stats_before = runtime.execution_stats();
    let reused = runtime
        .run_script(
            SourceInput::from_javascript(
                "JSON.stringify([evalEnvCaller(evalEnvTarget, 4), evalEnvTarget(5)]);",
            ),
            "jit-machine-eval-env-direct-call-reuse.js",
        )
        .expect("eval-env closure survives factory frame and full GC")
        .completion_string()
        .to_owned();
    assert_eq!(reused, "[44,45]");
    assert!(runtime.execution_stats().jit_generated_calls > stats_before.jit_generated_calls);
}

#[test]
fn nested_machine_calls_rewrite_live_roots_during_gc() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_osr_threshold(u32::MAX)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("nested Machine direct-call runtime");
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(NESTED_GC_SETUP),
            "jit-machine-nested-gc-setup.js",
        )
        .expect("nested Machine direct-call setup");
    let used_machine_direct_call = setup.jit_artifacts().is_some_and(|batch| {
        batch.bundles().iter().any(|bundle| {
            bundle
                .file(JitArtifactFileName::OptimizedIr)
                .is_some_and(|file| {
                    file.contents()
                        .starts_with(b"; backend=otter-machine-ir scalar-function\n")
                })
                && bundle
                    .file(JitArtifactFileName::Relocations)
                    .is_some_and(|file| {
                        std::str::from_utf8(file.contents())
                            .is_ok_and(|text| text.contains("directCallEntryCell"))
                    })
        })
    });
    let stats_before = runtime.execution_stats();
    let gc_before = runtime.heap_stats().minor_gc_cycles;
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(NESTED_GC_PROBE),
            "jit-machine-nested-gc-probe.js",
        )
        .expect("nested Machine direct-call probe")
        .completion_string()
        .to_owned();
    let stats_after = runtime.execution_stats();

    assert_eq!(completion, "kept:17100000");
    assert!(
        used_machine_direct_call,
        "setup must publish direct Machine IR"
    );
    assert!(
        stats_after.jit_generated_calls > stats_before.jit_generated_calls,
        "probe must enter nested generated callees"
    );
    assert!(
        runtime.heap_stats().minor_gc_cycles > gc_before,
        "allocating callee must collect while caller roots are published"
    );
    runtime
        .force_gc()
        .expect("completed Machine calls must leave no stale root record");
    let reused = runtime
        .run_script(
            SourceInput::from_javascript(r#"outer(middle, allocator, "again:", 0);"#),
            "jit-machine-nested-gc-reuse.js",
        )
        .expect("Machine caller must remain reusable after full GC")
        .completion_string()
        .to_owned();
    assert_eq!(reused, "again:0");
}

#[test]
fn own_method_binds_exact_receiver_through_machine_ir() {
    let oracle = run(
        OWN_METHOD,
        "jit-machine-own-method.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        OWN_METHOD,
        "jit-machine-own-method.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[42,40]");
    assert_machine_method_call(&compiled);
}

#[test]
fn prototype_method_guard_binds_exact_receiver() {
    let oracle = run(
        PROTOTYPE_METHOD,
        "jit-machine-prototype-method.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        PROTOTYPE_METHOD,
        "jit-machine-prototype-method.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[42,40]");
    assert_machine_method_call(&compiled);
}

#[test]
fn method_cold_exits_do_not_replay_and_caller_is_reusable() {
    let oracle = run(
        METHOD_COLD_EXITS,
        "jit-machine-method-cold-exits.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        METHOD_COLD_EXITS,
        "jit-machine-method-cold-exits.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, r#"[2147483648,"method-boom",5,1,42]"#);
    assert_machine_method_call(&compiled);
    assert!(compiled.stats.jit_generated_call_deopts > 0);
}

#[test]
fn method_receiver_arguments_and_deopt_state_can_spill() {
    let oracle = run(
        METHOD_SPILLS,
        "jit-machine-method-spills.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        METHOD_SPILLS,
        "jit-machine-method-spills.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "spill:115");
    assert_machine_method_call(&compiled);
}

#[test]
fn method_throw_enters_explicit_machine_landing_pad() {
    let oracle = run(
        METHOD_LANDING_PAD,
        "jit-machine-method-landing-pad.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        METHOD_LANDING_PAD,
        "jit-machine-method-landing-pad.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, r#"[8,[true,"landing-boom",-5],42,3]"#);
    assert_machine_method_call(&compiled);
    assert!(
        compiled.used_machine_method_landing_ack,
        "landingCaller must own the generated method linkage and its dedicated caught-throw acknowledgement: {:?}",
        compiled.compile_diagnostics
    );
}

#[test]
fn method_receiver_remains_rooted_during_moving_gc() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_osr_threshold(u32::MAX)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("Machine method-GC runtime");
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(METHOD_GC_SETUP),
            "jit-machine-method-gc-setup.js",
        )
        .expect("Machine method-GC setup");
    let used_machine_method_call = setup.jit_artifacts().is_some_and(|batch| {
        batch.bundles().iter().any(|bundle| {
            bundle
                .file(JitArtifactFileName::OptimizedIr)
                .is_some_and(|file| {
                    file.contents()
                        .starts_with(b"; backend=otter-machine-ir scalar-function\n")
                })
                && bundle
                    .file(JitArtifactFileName::Relocations)
                    .is_some_and(|file| {
                        std::str::from_utf8(file.contents())
                            .is_ok_and(|text| text.contains("\"callKind\": \"method\""))
                    })
        })
    });
    let stats_before = runtime.execution_stats();
    let gc_before = runtime.heap_stats().minor_gc_cycles;
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(METHOD_GC_PROBE),
            "jit-machine-method-gc-probe.js",
        )
        .expect("Machine method-GC probe")
        .completion_string()
        .to_owned();
    let stats_after = runtime.execution_stats();

    assert_eq!(completion, "method-root:100000");
    assert!(used_machine_method_call);
    assert!(stats_after.jit_generated_calls > stats_before.jit_generated_calls);
    assert!(runtime.heap_stats().minor_gc_cycles > gc_before);
    runtime
        .force_gc()
        .expect("completed method call must unlink its Machine root record");
    let reused = runtime
        .run_script(
            SourceInput::from_javascript("methodGcCaller(methodGcReceiver, 0);"),
            "jit-machine-method-gc-reuse.js",
        )
        .expect("Machine method caller must remain reusable after full GC")
        .completion_string()
        .to_owned();
    assert_eq!(reused, "method-root:0");
}

#[test]
fn recursive_and_mutually_recursive_machine_calls_use_stable_cells() {
    let oracle = run(
        RECURSIVE_CALLS,
        "jit-machine-recursive-calls.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        RECURSIVE_CALLS,
        "jit-machine-recursive-calls.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[0,1,2]");
    assert_machine_direct_call(&compiled);
}
