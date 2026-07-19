//! Production JIT direct-call identity and lifecycle regression coverage.
//!
//! # Contents
//! - Alternating closure instances from one function literal through one
//!   compiled plain-call site.
//! - Polymorphic receiver shapes and method identities through one compiled
//!   caller's semantic fallback.
//! - A monomorphic production-tier method site whose guarded splice removes
//!   the compiled-call boundary entirely.
//! - A prototype-held method whose inline body reads an own receiver property.
//! - An inline method whose hoisted local observes function-entry `undefined`.
//! - Compact scratch reuse across two arguments, assigned locals, and an
//!   overlapping parameter-assignment snapshot.
//! - Guarded numeric method splicing plus exact pre-effect side exits after
//!   receiver, callee, bound-state, or late operand guards miss.
//! - Callees that allocate their own captured cells while retaining the current
//!   receiver across moving-GC safepoints.
//! - Frameless direct callees that mint distinct capture-free function values.
//! - Recursive calls that catch or propagate throws before the same runtime
//!   successfully enters an independent compiler-generated native call.
//!
//! # Invariants
//! - A resolved compiled target never retains closure-owned capture state;
//!   SELF and upvalues are selected from the current callee on every call.
//! - Method dispatch selects both the current function and current receiver;
//!   neither method identity nor `this` leaks between polymorphic calls.
//! - A production-tier inline candidate eliminates the generated call boundary
//!   while preserving the same result as the interpreter.
//! - A template-spliced method body runs only for the exact baked receiver and
//!   method identity; every rejected guard preserves full method-call semantics.
//! - Scratch-register trimming preserves the entry `undefined` state of every
//!   local that the accepted inline body can read before writing.
//! - Compact slots never alias simultaneously live values; parameter
//!   reassignment cannot overwrite an earlier expression snapshot.
//! - Cold upvalue-spine construction roots inherited cells and dynamic call
//!   state until the new frame is published.
//! - Capture-free function construction uses the published SELF/register
//!   window and never requires a materialized interpreter frame.
//! - Return, caught-throw, and escaping-throw completion release every nested
//!   call lifecycle resource so later compiled entries remain reusable.
//! - Loop OSR is disabled, so observed direct calls cross whole-function
//!   compiled entries rather than an independently tiered loop body.
//!
//! # See also
//! - `jit_exception_regions.rs` — structured exception completion coverage.
//! - `jit_nested_exception.rs` — nested compiled handler precedence.

use otter_runtime::{JitSelection, Runtime, SourceInput};

struct RunResult {
    completion: String,
    compile_attempts: u64,
    osr_attempts: u64,
    reentrant_transitions: u64,
    generated_calls: u64,
}

struct SplitRunResult {
    completion: String,
    compile_attempts: u64,
    runtime_stub_delta: u64,
}

fn run(source: &str, name: &str, selection: JitSelection) -> RunResult {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(SourceInput::from_javascript(source.to_string()), name)
        .expect("call lifecycle fixture")
        .completion_string()
        .to_owned();
    let stats = runtime.execution_stats();
    RunResult {
        completion,
        compile_attempts: stats.jit_compile_attempts,
        osr_attempts: stats.jit_osr_attempts,
        reentrant_transitions: stats.jit_reentrant_stub_transitions,
        generated_calls: stats.jit_generated_calls,
    }
}

fn run_after_warmup(
    setup: &str,
    probe: &str,
    name: &str,
    selection: JitSelection,
) -> SplitRunResult {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX)
        .build()
        .expect("runtime");
    let setup_name = format!("{name}-setup.js");
    runtime
        .run_script(SourceInput::from_javascript(setup.to_string()), &setup_name)
        .expect("method-inline warmup");
    let before = runtime.execution_stats();
    let probe_name = format!("{name}-probe.js");
    let completion = runtime
        .run_script(SourceInput::from_javascript(probe.to_string()), &probe_name)
        .expect("method-inline probe")
        .completion_string()
        .to_owned();
    let after = runtime.execution_stats();
    SplitRunResult {
        completion,
        compile_attempts: before.jit_compile_attempts,
        runtime_stub_delta: after
            .jit_runtime_stub_transitions
            .saturating_sub(before.jit_runtime_stub_transitions),
    }
}

#[cfg(target_arch = "aarch64")]
fn assert_inline_method_probe(
    setup: &str,
    probe: &str,
    name: &str,
    expected: &str,
    expect_inline_hit: bool,
) {
    let oracle = run_after_warmup(setup, probe, name, JitSelection::InterpreterOnly);
    let compiled = run_after_warmup(setup, probe, name, JitSelection::ProductionTiered);
    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, expected);
    assert!(
        compiled.compile_attempts > 0,
        "{name} must compile the warmed method caller"
    );
    if expect_inline_hit {
        assert_eq!(
            compiled.runtime_stub_delta, 0,
            "{name} must complete through the spliced method body"
        );
    }
}

fn assert_whole_function_direct_calls(result: &RunResult) {
    assert_whole_function_compiled(result);
    assert!(
        result.generated_calls > 0,
        "fixture must cross a native compiled-call boundary"
    );
}

fn assert_whole_function_compiled(result: &RunResult) {
    assert!(
        result.compile_attempts > 0,
        "fixture must compile whole-function entries"
    );
    assert_eq!(
        result.osr_attempts, 0,
        "fixture must not tier through loop OSR"
    );
}

const CLOSURE_INSTANCES: &str = r#"
function makeCaptured(offset) {
  return function captured(value) {
    return offset + value;
  };
}

function callPlain(fn, value) {
  return fn(value);
}

const positive = makeCaptured(1000);
const negative = makeCaptured(-2000);

// Both callees originate from the same function literal and therefore share
// one function id. Alternate them until both the site and target are native.
for (let i = 0; i < 192; i++) {
  callPlain((i & 1) === 0 ? positive : negative, i);
}

let checksum = 0;
const trace = [];
for (let i = 0; i < 256; i++) {
  const value = callPlain((i & 1) === 0 ? positive : negative, i);
  checksum += value;
  if (i < 8) trace.push(value);
}

JSON.stringify([checksum, trace]);
"#;

#[test]
fn same_function_id_closures_keep_current_capture_state() {
    let oracle = run(
        CLOSURE_INSTANCES,
        "jit-call-closure-instances.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        CLOSURE_INSTANCES,
        "jit-call-closure-instances.js",
        JitSelection::Template,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(
        compiled.completion,
        "[-95360,[1000,-1999,1002,-1997,1004,-1995,1006,-1993]]"
    );
    assert_whole_function_direct_calls(&compiled);
}

const POLYMORPHIC_METHODS: &str = r#"
function add(value) {
  return this.base + value;
}
function multiply(value) {
  return this.base * value;
}
function subtract(value) {
  return this.base - value;
}
function callMethod(receiver, value) {
  return receiver.apply(value);
}

const addReceiver = { base: 100, apply: add };
const multiplyReceiver = { kind: "multiply", base: 7, apply: multiply };
const subtractFirst = { base: 500, apply: subtract };
const subtractSecond = { marker: true, base: 800, apply: subtract };
const receivers = [
  addReceiver,
  multiplyReceiver,
  subtractFirst,
  subtractSecond
];

// One source call site sees different shapes, different methods, and two
// instances sharing one method implementation but requiring different this.
for (let i = 0; i < 320; i++) {
  callMethod(receivers[i & 3], i);
}

const trace = [];
for (let i = 0; i < 12; i++) {
  trace.push(callMethod(receivers[i & 3], i));
}
JSON.stringify(trace);
"#;

#[test]
fn polymorphic_method_site_keeps_current_method_and_this() {
    let oracle = run(
        POLYMORPHIC_METHODS,
        "jit-call-polymorphic-methods.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        POLYMORPHIC_METHODS,
        "jit-call-polymorphic-methods.js",
        JitSelection::Template,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(
        compiled.completion,
        "[100,7,498,797,104,35,494,793,108,63,490,789]"
    );
    assert_whole_function_compiled(&compiled);
}

const OPTIMIZING_METHOD_CACHE: &str = r#"
function apply(value) {
  return value + this.bias;
}
function callMethod(receiver, value) {
  return receiver.apply(value);
}

const receiver = { bias: 4, apply };
for (let i = 0; i < 5000; i++) {
  callMethod(receiver, i);
}

let checksum = 0;
for (let i = 0; i < 256; i++) {
  checksum += callMethod(receiver, i);
}
JSON.stringify([checksum, callMethod(receiver, 9)]);
"#;

#[cfg(target_arch = "aarch64")]
#[test]
fn production_method_inline_eliminates_compiled_call_boundary() {
    let oracle = run(
        OPTIMIZING_METHOD_CACHE,
        "jit-call-optimizing-method-cache.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        OPTIMIZING_METHOD_CACHE,
        "jit-call-optimizing-method-cache.js",
        JitSelection::ProductionTiered,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[33664,13]");
    assert!(compiled.compile_attempts > 0, "fixture must compile");
    assert_eq!(compiled.osr_attempts, 0, "fixture must not use loop OSR");
    assert_eq!(
        compiled.generated_calls, 0,
        "spliced method calls must eliminate the generated call boundary"
    );
}

const INLINE_METHOD_SETUP: &str = r#"
globalThis.__jitInlineMethodFixture = (() => {
  function apply(value) {
    return value + this.bias;
  }
  function callMethod(receiver, value) {
    return receiver.apply(value);
  }

  const receiver = { bias: 4, apply };
  for (let i = 0; i < 5000; i++) {
    callMethod(receiver, i);
  }
  return { apply, callMethod, receiver };
})();
"#;

#[cfg(target_arch = "aarch64")]
#[test]
fn numeric_method_inline_hit_avoids_runtime_transition() {
    assert_inline_method_probe(
        INLINE_METHOD_SETUP,
        r#"
const fixture = globalThis.__jitInlineMethodFixture;
JSON.stringify([
  fixture.callMethod(fixture.receiver, 9),
  fixture.callMethod(fixture.receiver, 20)
]);
"#,
        "jit-inline-method-hit",
        "[13,24]",
        true,
    );
}

const PROTOTYPE_INLINE_METHOD_SETUP: &str = r#"
globalThis.__jitPrototypeInlineMethodFixture = (() => {
  function apply(value) {
    return value + this.bias;
  }
  function callMethod(receiver, value) {
    return receiver.apply(value);
  }

  const prototype = { apply };
  const receiver = Object.create(prototype);
  receiver.bias = 4;
  for (let i = 0; i < 5000; i++) {
    callMethod(receiver, i);
  }
  return { callMethod, receiver };
})();
"#;

#[cfg(target_arch = "aarch64")]
#[test]
fn prototype_method_inline_reuses_guarded_receiver_property() {
    assert_inline_method_probe(
        PROTOTYPE_INLINE_METHOD_SETUP,
        r#"
const fixture = globalThis.__jitPrototypeInlineMethodFixture;
JSON.stringify([
  fixture.callMethod(fixture.receiver, 9),
  fixture.callMethod(fixture.receiver, 20)
]);
"#,
        "jit-inline-prototype-method-hit",
        "[13,24]",
        true,
    );
}

const UNINITIALIZED_LOCAL_INLINE_METHOD_SETUP: &str = r#"
globalThis.__jitUninitializedLocalInlineMethodFixture = (() => {
  function apply() {
    var missing;
    return missing;
  }
  function callMethod(receiver) {
    return receiver.apply();
  }

  const receiver = { apply };
  for (let i = 0; i < 5000; i++) {
    callMethod(receiver);
  }
  return { callMethod, receiver };
})();
"#;

#[cfg(target_arch = "aarch64")]
#[test]
fn method_inline_initializes_only_read_before_write_locals() {
    assert_inline_method_probe(
        UNINITIALIZED_LOCAL_INLINE_METHOD_SETUP,
        r#"
const fixture = globalThis.__jitUninitializedLocalInlineMethodFixture;
JSON.stringify([
  fixture.callMethod(fixture.receiver),
  fixture.callMethod(fixture.receiver)
]);
"#,
        "jit-inline-uninitialized-local-hit",
        "[null,null]",
        true,
    );
}

const COMPACT_INLINE_METHOD_SETUP: &str = r#"
globalThis.__jitCompactInlineMethodFixture = (() => {
  function apply(left, right) {
    let sum = left + right;
    return sum + this.bias;
  }
  function callMethod(receiver, left, right) {
    return receiver.apply(left, right);
  }

  const receiver = { bias: 4, apply };
  for (let i = 0; i < 5000; i++) {
    callMethod(receiver, i, 2);
  }
  return { callMethod, receiver };
})();
"#;

#[cfg(target_arch = "aarch64")]
#[test]
fn method_inline_compacts_two_arguments_and_assigned_local() {
    assert_inline_method_probe(
        COMPACT_INLINE_METHOD_SETUP,
        r#"
const fixture = globalThis.__jitCompactInlineMethodFixture;
JSON.stringify([
  fixture.callMethod(fixture.receiver, 3, 5),
  fixture.callMethod(fixture.receiver, 10, 13)
]);
"#,
        "jit-inline-compact-two-argument-hit",
        "[12,27]",
        true,
    );
}

const SNAPSHOT_INLINE_METHOD_SETUP: &str = r#"
globalThis.__jitSnapshotInlineMethodFixture = (() => {
  function apply(value) {
    return value + (value = 2) + this.bias;
  }
  function callMethod(receiver, value) {
    return receiver.apply(value);
  }

  const receiver = { bias: 4, apply };
  for (let i = 0; i < 5000; i++) {
    callMethod(receiver, i);
  }
  return { callMethod, receiver };
})();
"#;

#[cfg(target_arch = "aarch64")]
#[test]
fn method_inline_keeps_parameter_assignment_snapshot_live() {
    assert_inline_method_probe(
        SNAPSHOT_INLINE_METHOD_SETUP,
        r#"
const fixture = globalThis.__jitSnapshotInlineMethodFixture;
JSON.stringify([
  fixture.callMethod(fixture.receiver, 9),
  fixture.callMethod(fixture.receiver, 20)
]);
"#,
        "jit-inline-parameter-snapshot-hit",
        "[15,26]",
        true,
    );
}

const LATE_DEOPT_INLINE_METHOD_SETUP: &str = r#"
globalThis.__jitLateDeoptInlineMethodFixture = (() => {
  function apply(value, suffix) {
    const numeric = value + this.bias;
    return numeric + suffix;
  }
  function callMethod(receiver, value, suffix) {
    return receiver.apply(value, suffix);
  }

  const receiver = { bias: 4, apply };
  for (let i = 0; i < 5000; i++) {
    callMethod(receiver, i, 1);
  }
  return { callMethod, receiver };
})();
"#;

#[cfg(target_arch = "aarch64")]
#[test]
fn method_inline_side_exits_after_late_guard_overwrites_scratch() {
    assert_inline_method_probe(
        LATE_DEOPT_INLINE_METHOD_SETUP,
        r#"
const fixture = globalThis.__jitLateDeoptInlineMethodFixture;
JSON.stringify(fixture.callMethod(fixture.receiver, 3, "!"));
"#,
        "jit-inline-late-operand-miss",
        r#""7!""#,
        false,
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn numeric_method_inline_side_exits_after_receiver_shape_change() {
    assert_inline_method_probe(
        INLINE_METHOD_SETUP,
        r#"
const fixture = globalThis.__jitInlineMethodFixture;
fixture.receiver.extra = true;
JSON.stringify(fixture.callMethod(fixture.receiver, 11));
"#,
        "jit-inline-method-shape-miss",
        "15",
        false,
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn numeric_method_inline_side_exits_after_method_replacement() {
    assert_inline_method_probe(
        INLINE_METHOD_SETUP,
        r#"
const fixture = globalThis.__jitInlineMethodFixture;
fixture.receiver.apply = function replacement(value) {
  return value * 3;
};
JSON.stringify(fixture.callMethod(fixture.receiver, 7));
"#,
        "jit-inline-method-identity-miss",
        "21",
        false,
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn numeric_method_inline_rejects_bound_closure_state() {
    assert_inline_method_probe(
        INLINE_METHOD_SETUP,
        r#"
const fixture = globalThis.__jitInlineMethodFixture;
fixture.receiver.apply = fixture.apply.bind({ bias: 1000 });
JSON.stringify(fixture.callMethod(fixture.receiver, 6));
"#,
        "jit-inline-method-bound-miss",
        "1006",
        false,
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn numeric_method_inline_side_exits_nonnumeric_operands() {
    assert_inline_method_probe(
        INLINE_METHOD_SETUP,
        r#"
const fixture = globalThis.__jitInlineMethodFixture;
JSON.stringify(fixture.callMethod(fixture.receiver, "value="));
"#,
        "jit-inline-method-operand-miss",
        r#""value=4""#,
        false,
    );
}

const UPVALUE_FRAME_BUILD: &str = r#"
function makeReader(value) {
  let captured = this.base + value;
  return function readCaptured() {
    return captured;
  };
}

function callFactory(receiver, value) {
  return receiver.make(value)();
}

const small = { base: 10, make: makeReader };
const large = { family: "large", base: 1000, make: makeReader };
for (let i = 0; i < 320; i++) {
  callFactory((i & 1) === 0 ? small : large, i);
}

let checksum = 0;
const trace = [];
for (let i = 0; i < 128; i++) {
  const value = callFactory((i & 1) === 0 ? small : large, i);
  checksum += value;
  if (i < 8) trace.push(value);
}
JSON.stringify([checksum, trace]);
"#;

#[test]
fn own_upvalue_fallback_roots_receiver_and_new_cells() {
    let oracle = run(
        UPVALUE_FRAME_BUILD,
        "jit-call-upvalue-frame-build.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        UPVALUE_FRAME_BUILD,
        "jit-call-upvalue-frame-build.js",
        JitSelection::Template,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(
        compiled.completion,
        "[72768,[10,1001,12,1003,14,1005,16,1007]]"
    );
    assert_whole_function_compiled(&compiled);
}

const FRAMELESS_MAKE_FUNCTION: &str = r#"
function mint() {
  return function leaf(value) {
    return value + 1;
  };
}

function mintThroughCompiledCaller() {
  return mint();
}

// Compile both endpoints, then keep allocating distinct capture-free closure
// bodies through a compiled-to-compiled call. The volume is intentional: it
// exercises relocation while MakeFunction's destination stays in the native
// owner's published register window.
let warmChecksum = 0;
for (let i = 0; i < 512; i++) {
  warmChecksum += mintThroughCompiledCaller()(i);
}

const first = mintThroughCompiledCaller();
const second = mintThroughCompiledCaller();
JSON.stringify([
  first !== second,
  first(40),
  second(41),
  warmChecksum
]);
"#;

#[test]
fn frameless_make_function_mints_distinct_values_across_gc() {
    let oracle = run(
        FRAMELESS_MAKE_FUNCTION,
        "jit-call-frameless-make-function.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        FRAMELESS_MAKE_FUNCTION,
        "jit-call-frameless-make-function.js",
        JitSelection::Template,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[true,41,42,131328]");
    assert_whole_function_direct_calls(&compiled);
}

const RECURSIVE_THROW_CLEANUP: &str = r#"
function recursive(depth, mode, state) {
  state.entries++;
  if (depth === 0) {
    if (mode !== 0) {
      state.throws++;
      throw { mode: mode, ordinal: state.throws };
    }
    return 1;
  }
  return recursive(depth - 1, mode, state) + 1;
}

function catchInside(depth, state) {
  try {
    return recursive(depth, 1, state);
  } catch (error) {
    state.caught++;
    return 1000 + error.mode;
  }
}

function letEscape(depth, state) {
  return recursive(depth, 2, state);
}

function stableAdd(value) {
  if (value < 0) return value - 1;
  return value + 1;
}

function succeed(depth, state) {
  return stableAdd(depth);
}

const warm = { entries: 0, throws: 0, caught: 0 };
for (let i = 0; i < 96; i++) catchInside(8, warm);
for (let i = 0; i < 96; i++) {
  try {
    letEscape(7, warm);
  } catch (error) {
    // The fixture intentionally lets the throw escape every compiled frame.
  }
}
for (let i = 0; i < 96; i++) stableAdd(i);
for (let i = 0; i < 96; i++) succeed(9, warm);

const state = { entries: 0, throws: 0, caught: 0, escaped: 0 };
let caughtSum = 0;
let escapedCodeSum = 0;
let successSum = 0;
for (let i = 0; i < 128; i++) {
  caughtSum += catchInside(8, state);
  try {
    letEscape(7, state);
  } catch (error) {
    state.escaped++;
    escapedCodeSum += error.mode;
  }
  // This call immediately follows both cleanup paths on every iteration.
  successSum += succeed(9, state);
}

JSON.stringify([
  caughtSum,
  escapedCodeSum,
  successSum,
  state.entries,
  state.throws,
  state.caught,
  state.escaped
]);
"#;

#[test]
fn recursive_throw_cleanup_leaves_compiled_state_reusable() {
    let oracle = run(
        RECURSIVE_THROW_CLEANUP,
        "jit-call-recursive-throw-cleanup.js",
        JitSelection::InterpreterOnly,
    );
    let compiled = run(
        RECURSIVE_THROW_CLEANUP,
        "jit-call-recursive-throw-cleanup.js",
        JitSelection::Template,
    );

    assert_eq!(compiled.completion, oracle.completion);
    assert_eq!(compiled.completion, "[128128,256,1280,2176,256,128,128]");
    assert_whole_function_direct_calls(&compiled);
    assert!(
        compiled.reentrant_transitions > 0,
        "throw completion must use the shared reentrant transition"
    );
}
