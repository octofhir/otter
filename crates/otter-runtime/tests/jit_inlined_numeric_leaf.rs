//! Template-inlined numeric-leaf production-entry invariants.
//!
//! # Contents
//! - A hot eight-operation Number leaf called through a compiled caller.
//! - Full collections before and after non-Number numeric inputs.
//! - Successful and throwing `Symbol.toPrimitive` completion without replay.
//!
//! # Invariants
//! - The template inliner keeps the monomorphic leaf in the caller activation.
//!   Once the caller reaches the optimizing tier, its protected call enters
//!   the optimizing leaf through generated linkage and completes there.
//! - A non-Number input runs coercion exactly once, so abrupt completion keeps
//!   its identity. The inlined template leaf coerces in place; the optimizing
//!   leaf takes one exact pre-coercion exit on the first such input.
//! - The compiled caller, inlined leaf, globals, and retained results remain
//!   reusable across full moving collections.
//!
//! # See also
//! - `optimizing_leaf_deopt.rs` covers optimizing-tier guard exits.
//! - `jit_call_lifecycle.rs` covers compiled direct-call ownership.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitDebugEvent, JitDebugRequest, JitDebugTier, JitSelection, Runtime, RuntimeExecutionStats,
    SourceInput,
};
use otter_vm::JitDirectCallLoweringOutcome;

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

// Native iteration drives every warm call through a whole-function entry: a
// JavaScript warm loop would itself tier up through OSR and call the caller
// through generated linkage.
globalThis.__numericWarm = new Array(4300)
  .fill(2)
  .map(numericCaller)
  .reduce((total, value) => total + value, 0);
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
    used_template_inline: bool,
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
    let capture_events = matches!(&selection, JitSelection::ProductionTiered);
    let builder = Runtime::builder().jit_selection(selection);
    let mut runtime = if capture_events {
        builder.jit_debug(JitDebugRequest::events()).build()
    } else {
        builder.build()
    }
    .expect("numeric-leaf runtime");
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(SETUP),
            "jit-inlined-numeric-leaf-setup.js",
        )
        .expect("numeric-leaf setup fixture");
    let used_template_inline = setup.jit_debug_report().is_some_and(|report| {
        report.events().iter().any(|event| {
            matches!(
                event,
                JitDebugEvent::DirectCallLowered {
                    tier: JitDebugTier::Template,
                    outcome: JitDirectCallLoweringOutcome::Inlined,
                    ..
                }
            )
        })
    });
    force_full_gc(&mut runtime);
    eval(
        &mut runtime,
        GUARD_MISSES,
        "jit-inlined-numeric-leaf-guard-misses.js",
    );
    force_full_gc(&mut runtime);
    let completion = eval(&mut runtime, PROBE, "jit-inlined-numeric-leaf-probe.js");
    RunResult {
        completion,
        stats: runtime.execution_stats(),
        used_template_inline,
    }
}

#[test]
fn production_inline_full_gc_and_nested_abrupt_exit_stay_reusable() {
    let compiled = run(JitSelection::ProductionTiered);

    assert_eq!(compiled.completion, "[-30100,-7,701,-7,1,1,true,true,true]");
    assert!(
        compiled.used_template_inline,
        "production tiering must inline the hot monomorphic leaf: {:?}",
        compiled.stats
    );
    // The optimizing `numericCaller` keeps its call inside `try` as generated
    // linkage to a native leaf entry; every such call completes there.
    let stats = &compiled.stats;
    assert_eq!(
        stats.jit_generated_calls,
        stats.jit_generated_optimizing_entries + stats.jit_generated_template_entries,
        "{stats:?}"
    );
    assert_eq!(
        stats.jit_generated_optimizing_entries,
        stats.jit_generated_optimizing_returns
            + stats.jit_generated_optimizing_throws
            + stats.jit_generated_optimizing_deopts,
        "{stats:?}"
    );
    assert_eq!(
        stats.jit_generated_template_entries,
        stats.jit_generated_template_returns + stats.jit_generated_template_throws,
        "{stats:?}"
    );
    // The first non-Number input exits the optimizing leaf before coercion
    // once; the recompiled leaf then completes every call in place.
    assert_eq!(stats.jit_generated_optimizing_deopts, 1, "{stats:?}");
    assert_eq!(compiled.stats.jit_generated_call_deopts, 1);
    assert_eq!(compiled.stats.jit_generated_template_deopts, 0);
    assert_eq!(compiled.stats.jit_to_rust_call_transitions, 0);
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
