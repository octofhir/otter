//! Production JIT direct-call identity and lifecycle regression coverage.
//!
//! # Contents
//! - Alternating closure instances from one function literal through one
//!   compiled plain-call site.
//! - Polymorphic receiver shapes and method identities through one compiled
//!   method-call site.
//! - Direct callees that allocate their own captured cells while retaining the
//!   current receiver across moving-GC safepoints.
//! - Recursive compiled calls that catch or propagate throws before the same
//!   runtime successfully enters another compiled call.
//!
//! # Invariants
//! - A resolved compiled target never retains closure-owned capture state;
//!   SELF and upvalues are selected from the current callee on every call.
//! - Method dispatch selects both the current function and current receiver;
//!   neither method identity nor `this` leaks between polymorphic calls.
//! - Upvalue-spine construction roots inherited cells and dynamic call state
//!   until the new frame is published.
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
    direct_calls: u64,
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
        direct_calls: stats.jit_direct_calls,
    }
}

fn assert_whole_function_direct_calls(result: &RunResult) {
    assert!(
        result.compile_attempts > 0,
        "fixture must compile whole-function entries"
    );
    assert_eq!(
        result.osr_attempts, 0,
        "fixture must not tier through loop OSR"
    );
    assert!(
        result.direct_calls > 0,
        "fixture must cross a compiled direct-call boundary"
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
    assert_whole_function_direct_calls(&compiled);
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
fn direct_frame_build_roots_receiver_and_new_upvalue_cells() {
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

function succeed(depth, state) {
  return recursive(depth, 0, state);
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
    assert_eq!(compiled.completion, "[128128,256,1280,3456,256,128,128]");
    assert_whole_function_direct_calls(&compiled);
    assert!(
        compiled.reentrant_transitions > 0,
        "throw completion must use the shared reentrant transition"
    );
}
