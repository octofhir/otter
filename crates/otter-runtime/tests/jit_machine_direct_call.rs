//! Machine IR direct-call execution coverage.
//!
//! # Contents
//! - Native publication proof for a monomorphic plain call.
//! - Exact return, callee-deopt, and throw semantics against the interpreter.
//! - Own/prototype guarded methods, exact receiver binding, and guard misses.
//! - Base constructors with `new.target`, receiver substitution, and accessors.
//! - Nested generated calls retaining a tagged value across moving GC.
//!
//! # Invariants
//! - The fixture must execute through the Machine IR backend; semantic equality
//!   alone may not be satisfied by the legacy optimizing fallback.
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
function landingMethod(value) {
  if (value < 0) throw "landing-boom";
  return value + 1;
}

const landingReceiver = { method: landingMethod };
function landingCaller(receiver, value) {
  let result = undefined;
  try {
    result = receiver.method(value);
  } catch {
    result = undefined;
  }
  return result;
}

for (let i = 0; i < 5000; i++) landingCaller(landingReceiver, i);
JSON.stringify([
  landingCaller(landingReceiver, 7),
  landingCaller(landingReceiver, -5),
  landingCaller(landingReceiver, 41)
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

struct RunResult {
    completion: String,
    stats: RuntimeExecutionStats,
    used_machine_direct_call: bool,
    used_machine_method_call: bool,
    used_machine_construct: bool,
}

fn run(source: &'static str, name: &'static str, selection: JitSelection) -> RunResult {
    let artifacts = matches!(selection, JitSelection::ProductionTiered);
    let builder = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX);
    let mut runtime = if artifacts {
        builder.jit_debug(JitDebugRequest::artifacts()).build()
    } else {
        builder.build()
    }
    .expect("Machine direct-call runtime");
    let result = runtime
        .run_script(SourceInput::from_javascript(source), name)
        .unwrap_or_else(|error| {
            panic!("Machine direct-call fixture {name} ({selection:?}): {error:?}")
        });
    let artifact_has = |needle: &str| {
        result.jit_artifacts().is_some_and(|batch| {
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
                                .is_ok_and(|text| text.contains(needle))
                        })
            })
        })
    };
    let used_machine_direct_call = artifact_has("directCallEntryCell");
    let used_machine_method_call =
        artifact_has("\"callKind\": \"method\"") || artifact_has("\"callKind\":\"method\"");
    let used_machine_construct =
        artifact_has("\"callKind\": \"construct\"") || artifact_has("\"callKind\":\"construct\"");
    RunResult {
        completion: result.completion_string().to_owned(),
        stats: runtime.execution_stats(),
        used_machine_direct_call,
        used_machine_method_call,
        used_machine_construct,
    }
}

fn assert_machine_construct(result: &RunResult) {
    assert_machine_direct_call(result);
    assert!(
        result.used_machine_construct,
        "fixture must publish a typed Machine IR construct target"
    );
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
        "fixture must enter a generated callee"
    );
    assert!(
        result.used_machine_direct_call,
        "fixture must publish a Machine IR body containing direct linkage"
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
    assert_eq!(compiled.completion, "[8,null,42]");
    assert_machine_method_call(&compiled);
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
