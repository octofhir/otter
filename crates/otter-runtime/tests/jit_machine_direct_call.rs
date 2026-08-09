//! Machine IR direct-call execution coverage.
//!
//! # Contents
//! - Native publication proof for a monomorphic plain call.
//! - Exact return, callee-deopt, and throw semantics against the interpreter.
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

struct RunResult {
    completion: String,
    stats: RuntimeExecutionStats,
    used_machine_direct_call: bool,
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
        .expect("Machine direct-call fixture");
    let used_machine_direct_call = result.jit_artifacts().is_some_and(|batch| {
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
    RunResult {
        completion: result.completion_string().to_owned(),
        stats: runtime.execution_stats(),
        used_machine_direct_call,
    }
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
