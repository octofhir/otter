//! Machine error allocation, coercion and generated-stack diagnostics.
//!
//! # Contents
//! - Intrinsic error creation after native call-chain tier-up.
//! - Observable message coercion, moving roots and complete constructor stacks.
//! - Pure native throws, local catch edges and materialized-frame diagnostics.
//!
//! # Invariants
//! - Tests require optimizing artifacts, not merely interpreter-equivalent output.
//! - Error construction and message effects occur once across native boundaries.

use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};

#[test]
fn machine_error_allocation_preserves_native_callers_and_message_effects() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var createNow = false;
function makeError(message) { if (!createNow) return 1; return new Error(message); }
function middleError(message) { return makeError(message); }
function outerError(message) { return middleError(message); }
function makeTypeError(message) { if (!createNow) return 1; return new TypeError(message); }
function deoptBridge(value) { return outerError(value + 1); }
function nativeBridge(value) { return deoptBridge(value); }
var warmErrors = 0;
for (var errorI = 0; errorI < 150000; errorI++) {
  warmErrors += outerError("unused") + makeTypeError("unused") + nativeBridge(1);
}
warmErrors;
"#,
            ),
            "machine-error-warm.js",
        )
        .expect("error factory warmup");
    assert_eq!(warm.completion_string(), "450000");
    for name in ["makeError", "makeTypeError", "deoptBridge", "nativeBridge"] {
        assert!(
            warm.jit_artifacts()
                .unwrap()
                .bundles()
                .iter()
                .any(|b| b.manifest().function_name() == name
                    && b.file(JitArtifactFileName::OptimizedIr).is_some()),
            "{name} must compile: {:?}",
            warm.jit_debug_report()
        );
    }
    let deopts_before = runtime.execution_stats().jit_generated_call_deopts;
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
createNow = true;
var messageEffects = 0;
var message = {toString: function() {
  messageEffects++;
  var moving = Array(64).fill({tag: "live"});
  return moving[63].tag;
}};
var err = outerError(message);
var typed = makeTypeError("typed");
var trace = err.stack;
var coercionTrace = "";
nativeBridge({valueOf: function() {
  coercionTrace = outerError("coercion").stack;
  return 1;
}});
JSON.stringify([err instanceof Error, err.message, messageEffects,
  typed instanceof TypeError, typed.message,
  trace.includes("makeError"), trace.includes("middleError"), trace.includes("outerError"),
  coercionTrace.includes("nativeBridge"), coercionTrace.split("deoptBridge").length - 1]);
"#,
            ),
            "machine-error-probe.js",
        )
        .expect("native error construction");
    assert_eq!(
        result.completion_string(),
        "[true,\"live\",1,true,\"typed\",true,true,true,true,1]"
    );
    assert!(
        runtime.execution_stats().jit_generated_call_deopts > deopts_before,
        "coercion stack must include a genuinely materialized native frame"
    );
    let error = runtime
        .run_script(
            SourceInput::from_javascript(
                "outerError({toString: function messageFailure() { throw 'coerce'; }});",
            ),
            "machine-error-coercion-throw.js",
        )
        .expect_err("coercion throws once");
    let otter_runtime::OtterError::Runtime { diagnostic } = error else {
        panic!("runtime diagnostic")
    };
    assert_eq!(diagnostic.frames[0].function, "messageFailure");
    for name in ["makeError", "middleError", "outerError"] {
        assert_eq!(
            diagnostic
                .frames
                .iter()
                .filter(|frame| frame.function == name)
                .count(),
            1,
            "complete native coercion stack: {:?}",
            diagnostic.frames
        );
    }
}

#[test]
fn machine_throw_preserves_values_and_local_catch_edges() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let warm = runtime.run_script(SourceInput::from_javascript(r#"
function throwValue(flag, value) { if (flag) throw value; return value; }
function catchValue(flag, value) { try { return throwValue(flag, value); } catch (caught) { return caught; } }
function localThrow(flag, value) { try { if (flag) throw value; return value; } catch (caught) { return caught; } }
function nativeThrowCaller(flag, value) { return throwValue(flag, value); }
var throwWarm = 0;
for (var throwWarmI = 0; throwWarmI < 70000; throwWarmI++) {
  throwWarm += catchValue(false, 1) + localThrow(false, 1) + nativeThrowCaller(false, 1);
}
throwWarm;
"#), "machine-throw-warm.js").expect("throw warmup");
    assert_eq!(warm.completion_string(), "210000");
    for name in [
        "throwValue",
        "catchValue",
        "localThrow",
        "nativeThrowCaller",
    ] {
        assert!(
            warm.jit_artifacts()
                .unwrap()
                .bundles()
                .iter()
                .any(|b| b.manifest().function_name() == name
                    && b.file(JitArtifactFileName::OptimizedIr).is_some()),
            "{name} must compile: {:?}",
            warm.jit_debug_report()
        );
    }
    let before = runtime.execution_stats();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
var thrownValues = [undefined, null, true, 1, -0, 1.5, NaN, "text", 3n, Symbol("s"), {tag: 1}];
var throwMatches = 0;
for (var throwProbeI = 0; throwProbeI < 128; throwProbeI++) {
  var thrownValue = thrownValues[throwProbeI % thrownValues.length];
  if (Object.is(catchValue(true, thrownValue), thrownValue)) throwMatches++;
  if (Object.is(localThrow(true, thrownValue), thrownValue)) throwMatches++;
}
throwMatches;
"#,
            ),
            "machine-throw-probe.js",
        )
        .expect("pure exception identity");
    assert_eq!(result.completion_string(), "256");
    let after = runtime.execution_stats();
    assert!(after.jit_generated_calls - before.jit_generated_calls >= 128);
    let error = runtime
        .run_script(
            SourceInput::from_javascript("nativeThrowCaller(true, {tag: 'uncaught'});"),
            "machine-uncaught-probe.js",
        )
        .expect_err("native throw escapes");
    let otter_runtime::OtterError::Runtime { diagnostic } = error else {
        panic!("runtime diagnostic")
    };
    assert_eq!(diagnostic.frames[0].function, "throwValue");
    assert_eq!(diagnostic.frames[1].function, "nativeThrowCaller");
    assert!(diagnostic.frames[0].span.is_some());
}
