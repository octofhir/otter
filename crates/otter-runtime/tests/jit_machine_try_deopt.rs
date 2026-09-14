//! Machine exact-deoptimization inside statically reconstructable catch regions.
//!
//! # Contents
//! - A hot Int32 addition compiled as Machine IR inside `try`.
//! - A Symbol guard miss that resumes the canonical operation with its catch
//!   handler rebuilt, followed by a supported reuse call.
//!
//! # Invariants
//! - Exact deopt resumes at the original bytecode with the active catch stack
//!   restored before canonical execution.
//! - The throwing addition and catch body execute once; no generated edge
//!   invents an exception value owned by the interpreter.
//! - Loop-header OSR inside an active exception region remains disabled.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactFileName, JitDebugRequest, JitDebugTarget, JitDebugTier, JitSelection, Runtime,
    SourceInput,
};

const MODULE: &str = "jit-machine-try-deopt-setup.js";
const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";

const SETUP: &str = r#"
globalThis.__machineTryCatchCount = 0;
function machineCatchExactDeopt(value) {
  try {
    return value + 1;
  } catch (error) {
    __machineTryCatchCount++;
    return error.name;
  }
}

for (let warm = 0; warm < 5000; warm++) {
  machineCatchExactDeopt(warm & 7);
}
"#;

const PROBE: &str = r#"
const __machineTryBefore = __machineTryCatchCount;
const __machineTryError = machineCatchExactDeopt(Symbol("guard miss"));
const __machineTryAfter = __machineTryCatchCount;
const __machineTryReuse = machineCatchExactDeopt(7);
JSON.stringify([
  __machineTryError,
  __machineTryAfter - __machineTryBefore,
  __machineTryReuse
]);
"#;

const COLD_CALL_SETUP: &str = r#"
globalThis.__machineColdCallEffects = 0;
function machineColdThrow(value) {
  __machineColdCallEffects++;
  throw value;
}

function machineCatchColdCall(attempt, fn, value) {
  try {
    if (attempt) return fn(value);
    return value + 1;
  } catch (error) {
    return error;
  }
}

for (let warm = 0; warm < 5000; warm++) {
  machineCatchColdCall(false, machineColdThrow, warm & 7);
}
"#;

const COLD_CALL_PROBE: &str = r#"
const __machineColdBefore = __machineColdCallEffects;
const __machineColdCaught = machineCatchColdCall(true, machineColdThrow, 73);
const __machineColdAfter = __machineColdCallEffects;
const __machineColdReuse = machineCatchColdCall(false, machineColdThrow, 7);
JSON.stringify([
  __machineColdCaught,
  __machineColdAfter - __machineColdBefore,
  __machineColdReuse
]);
"#;

fn runtime(selection: JitSelection, artifacts: bool) -> Runtime {
    let builder = Runtime::builder().jit_selection(selection);
    if artifacts {
        builder.jit_debug(JitDebugRequest::artifacts()).build()
    } else {
        builder.build()
    }
    .expect("try-deopt runtime")
}

fn completion(runtime: &mut Runtime, source: &str, module: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), module)
        .unwrap_or_else(|error| panic!("try-deopt fixture {module}: {error:?}"))
        .completion_string()
        .to_owned()
}

#[test]
fn exact_numeric_deopt_rebuilds_the_active_catch_before_resume() {
    let mut oracle = runtime(JitSelection::InterpreterOnly, false);
    completion(&mut oracle, SETUP, "jit-machine-try-deopt-oracle-setup.js");
    let expected = completion(&mut oracle, PROBE, "jit-machine-try-deopt-oracle-probe.js");
    assert_eq!(expected, r#"["TypeError",1,8]"#);

    let mut compiled = runtime(JitSelection::ProductionTiered, true);
    let setup = compiled
        .run_script(SourceInput::from_javascript(SETUP), MODULE)
        .expect("try-deopt compiled setup");
    let artifacts = setup
        .jit_artifacts()
        .expect("enabled try-deopt artifact batch");
    assert!(artifacts.bundles().iter().any(|bundle| {
        let manifest = bundle.manifest();
        manifest.module() == MODULE
            && manifest.function_name() == "machineCatchExactDeopt"
            && manifest.tier() == JitDebugTier::Optimizing
            && manifest.entry() == JitDebugTarget::Entry
            && bundle
                .file(JitArtifactFileName::OptimizedIr)
                .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
    }));
    drop(setup);

    let before = compiled.execution_stats();
    let actual = completion(
        &mut compiled,
        PROBE,
        "jit-machine-try-deopt-compiled-probe.js",
    );
    let after = compiled.execution_stats();
    assert_eq!(actual, expected);
    assert!(after.jit_optimized_entries > before.jit_optimized_entries);
    assert_eq!(
        after.jit_optimized_deopts - before.jit_optimized_deopts,
        1,
        "the Symbol guard must exact-deopt once before the caught canonical add"
    );
}

#[test]
fn never_attempted_call_deopt_rebuilds_the_catch_and_executes_once() {
    let mut oracle = runtime(JitSelection::InterpreterOnly, false);
    completion(
        &mut oracle,
        COLD_CALL_SETUP,
        "jit-machine-try-cold-call-oracle-setup.js",
    );
    let expected = completion(
        &mut oracle,
        COLD_CALL_PROBE,
        "jit-machine-try-cold-call-oracle-probe.js",
    );
    assert_eq!(expected, "[73,1,8]");

    let mut compiled = runtime(JitSelection::ProductionTiered, true);
    let setup = compiled
        .run_script(
            SourceInput::from_javascript(COLD_CALL_SETUP),
            "jit-machine-try-cold-call-setup.js",
        )
        .expect("try cold-call compiled setup");
    let artifacts = setup
        .jit_artifacts()
        .expect("enabled try cold-call artifact batch");
    assert!(artifacts.bundles().iter().any(|bundle| {
        let manifest = bundle.manifest();
        manifest.function_name() == "machineCatchColdCall"
            && manifest.tier() == JitDebugTier::Optimizing
            && bundle
                .file(JitArtifactFileName::OptimizedIr)
                .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
            && bundle
                .file(JitArtifactFileName::CodeMap)
                .is_some_and(|file| {
                    std::str::from_utf8(file.contents())
                        .is_ok_and(|text| text.contains("machineColdCallExit"))
                })
    }));
    drop(setup);

    let before = compiled.execution_stats();
    let actual = completion(
        &mut compiled,
        COLD_CALL_PROBE,
        "jit-machine-try-cold-call-probe.js",
    );
    let after = compiled.execution_stats();
    assert_eq!(actual, expected);
    assert_eq!(
        after.jit_optimized_deopts - before.jit_optimized_deopts,
        1,
        "the first attempted cold call must deopt once before canonical effect-once execution"
    );
}
