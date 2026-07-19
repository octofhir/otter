#![cfg(target_arch = "aarch64")]

//! Cranelift numeric-leaf production-entry invariants.
//!
//! # Contents
//! - A hot eight-operation Number leaf called through a compiled caller.
//! - Full collections before and after speculative Number guard misses.
//! - Successful and throwing `Symbol.toPrimitive` completion after cold deopt.
//!
//! # Invariants
//! - Cranelift remains the internal backend of the existing optimizing tier.
//! - A non-Number guard miss restarts at PC zero before observable effects, so
//!   coercion runs exactly once and abrupt completion keeps its identity.
//! - The compiled caller, optimized leaf, globals, and retained results remain
//!   reusable across full moving collections.
//!
//! # See also
//! - `optimizing_leaf_deopt.rs` covers the general AArch64 optimizer.
//! - `jit_call_lifecycle.rs` covers compiled direct-call ownership.

use otter_runtime::{
    JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    RuntimeExecutionStats, SourceInput,
};

const SETUP: &str = r#"
function numericLeaf(left, right) {
  var sum = left + right;
  var product = sum * right;
  var delta = product - right;
  var offset = delta + right;
  var scaled = offset * right;
  var reduced = scaled - right;
  var quotient = reduced / right;
  return -quotient;
}

function numericCaller(value) {
  try {
    return numericLeaf(value, 2);
  } catch (error) {
    if (error === globalThis.__numericSentinel) {
      return 701;
    }
    throw error;
  }
}

globalThis.__numericSentinel = { marker: "numeric-sentinel" };
globalThis.__numericCoercions = 0;
globalThis.__numericThrows = 0;
globalThis.__numericCoercible = {
  [Symbol.toPrimitive]() {
    globalThis.__numericCoercions++;
    return 2;
  }
};
globalThis.__numericThrowing = {
  [Symbol.toPrimitive]() {
    globalThis.__numericThrows++;
    throw globalThis.__numericSentinel;
  }
};
globalThis.__numericLeaf = numericLeaf;
globalThis.__numericCaller = numericCaller;

var warm = 0;
for (var index = 0; index < 4300; index++) {
  warm += numericCaller(2);
}
globalThis.__numericWarm = warm;
"#;

const GUARD_MISSES: &str = r#"
globalThis.__numericRetained = {
  coerced: globalThis.__numericCaller(globalThis.__numericCoercible),
  caught: globalThis.__numericCaller(globalThis.__numericThrowing),
  after: globalThis.__numericCaller(2),
  coercible: globalThis.__numericCoercible,
  throwing: globalThis.__numericThrowing,
  sentinel: globalThis.__numericSentinel
};
"#;

const PROBE: &str = r#"
var state = globalThis.__numericRetained;
JSON.stringify([
  globalThis.__numericWarm,
  state.coerced,
  state.caught,
  state.after,
  globalThis.__numericCoercions,
  globalThis.__numericThrows,
  state.coercible === globalThis.__numericCoercible,
  state.throwing === globalThis.__numericThrowing,
  state.sentinel === globalThis.__numericSentinel
]);
"#;

struct RunResult {
    completion: String,
    stats: RuntimeExecutionStats,
    used_cranelift: bool,
}

fn eval(runtime: &mut Runtime, source: &'static str, name: &'static str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), name)
        .expect("numeric-leaf fixture")
        .completion_string()
        .to_owned()
}

fn force_full_gc(runtime: &mut Runtime) {
    let cycles_before = runtime.heap_stats().gc_cycles;
    runtime.force_gc().expect("numeric-leaf full GC");
    assert!(
        runtime.heap_stats().gc_cycles > cycles_before,
        "fixture must execute a full collection"
    );
}

fn run(selection: JitSelection) -> RunResult {
    let capture_artifacts = matches!(&selection, JitSelection::ProductionTiered);
    let builder = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(u32::MAX);
    let mut runtime = if capture_artifacts {
        builder.jit_debug(JitDebugRequest::artifacts()).build()
    } else {
        builder.build()
    }
    .expect("numeric-leaf runtime");
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(SETUP),
            "cranelift-numeric-leaf-setup.js",
        )
        .expect("numeric-leaf setup fixture");
    let used_cranelift = setup.jit_artifacts().is_some_and(|batch| {
        batch.bundles().iter().any(|bundle| {
            bundle.manifest().tier() == JitDebugTier::Optimizing
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| {
                        file.contents()
                            .starts_with(b"; backend=cranelift numeric-leaf\n")
                    })
        })
    });
    force_full_gc(&mut runtime);
    eval(
        &mut runtime,
        GUARD_MISSES,
        "cranelift-numeric-leaf-guard-misses.js",
    );
    force_full_gc(&mut runtime);
    let completion = eval(&mut runtime, PROBE, "cranelift-numeric-leaf-probe.js");
    RunResult {
        completion,
        stats: runtime.execution_stats(),
        used_cranelift,
    }
}

#[test]
fn production_full_gc_guard_miss_and_nested_abrupt_exit_stay_reusable() {
    let compiled = run(JitSelection::ProductionTiered);

    assert_eq!(compiled.completion, "[-30100,-7,701,-7,1,1,true,true,true]");
    assert!(
        compiled.used_cranelift,
        "production tiering must route the hot leaf through Cranelift"
    );
    assert!(
        compiled.stats.jit_generated_optimizing_entries > 0,
        "numericLeaf must enter optimizing code from its generated caller: {:?}",
        compiled.stats
    );
    assert!(
        compiled.stats.jit_generated_optimizing_deopts >= 2,
        "both non-Number parameters must cold-deopt the generated call: {:?}",
        compiled.stats
    );
    assert!(
        compiled.stats.jit_generated_calls > 0,
        "the compiled caller must cross a direct native call boundary: {:?}",
        compiled.stats
    );
    assert_eq!(
        compiled.stats.jit_osr_attempts, 0,
        "the fixture isolates whole-function entries"
    );
}

#[test]
fn production_completion_matches_the_interpreter() {
    let oracle = run(JitSelection::InterpreterOnly);
    let compiled = run(JitSelection::ProductionTiered);

    assert_eq!(compiled.completion, oracle.completion);
}
