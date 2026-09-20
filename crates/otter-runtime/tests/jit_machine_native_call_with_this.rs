//! Generated native Math calls after explicit method lookup.
//!
//! # Contents
//! - `LoadProperty` / `CallWithThis` artifact and native-hit proofs.
//! - Lookup, argument, callable, receiver, and numeric completion semantics.
//! - Moving roots across argument evaluation and numeric coercion.
//! - Property preparation before the first optimizing namespace loop.
//!
//! # Invariants
//! - Arguments cannot cause a second lookup or replace the already-loaded callee.
//! - Admitted Int32 hits use generated code; cold completion executes once.
//! - Moving-GC probes retain bounded work and verify actual relocation.
//!
//! # See also
//! - `otter_jit::machine::numeric` for native call selection and cold CFG.
//! - `math_intrinsic_guards` for fused `CallMethodValue` coverage.

use otter_runtime::{
    ExecutionResult, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTier,
    JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};

const SETUP: &str = r#"
const explicitMath = { abs: Math.abs, min: Math.min, max: Math.max, marker: 41 };
function explicitAbs(receiver, value) { return receiver.abs(value | 0); }
function explicitMin(receiver, left, right) { return receiver.min(left | 0, right | 0); }
function explicitMax(receiver, left, right) { return receiver.max(left | 0, right | 0); }
function explicitEffect(receiver, argument) { return receiver.abs(argument() | 0); }
function initialArgument() { return -7; }
for (let warm = 0; warm < 4010; warm++) {
    explicitAbs(explicitMath, -7);
    explicitMin(explicitMath, -7, 2);
    explicitMax(explicitMath, -7, 2);
    explicitEffect(explicitMath, initialArgument);
}
"#;

fn run(runtime: &mut Runtime, source: &str) -> ExecutionResult {
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "native-call-with-this.js",
        )
        .unwrap_or_else(|error| panic!("explicit native call fixture: {error:?}"))
}

fn completion(runtime: &mut Runtime, source: &str) -> String {
    run(runtime, source).completion_string().to_owned()
}

fn runtime(selection: JitSelection) -> Runtime {
    Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .expect("explicit native call runtime")
}

fn artifact_text(bundle: &JitArtifactBundle, file: JitArtifactFileName) -> &str {
    std::str::from_utf8(
        bundle
            .file(file)
            .expect("captured artifact file")
            .contents(),
    )
    .expect("text artifact is UTF-8")
}

fn assert_explicit_native_artifact(result: &ExecutionResult, name: &str, sites: usize) {
    let bundles: Vec<_> = result
        .jit_artifacts()
        .expect("native call artifacts")
        .bundles()
        .iter()
        .filter(|bundle| {
            bundle.manifest().function_name() == name
                && bundle.manifest().tier() == JitDebugTier::Optimizing
        })
        .collect();
    assert_eq!(
        bundles.len(),
        1,
        "{name} must publish one optimizing body: {:?}",
        result.jit_debug_report()
    );
    let bundle = bundles[0];
    let bytecode = artifact_text(bundle, JitArtifactFileName::Bytecode);
    assert!(bytecode.contains("LoadProperty"), "{name}: {bytecode}");
    assert_eq!(
        bytecode.matches("CallWithThis").count(),
        sites,
        "{name}: {bytecode}"
    );
    assert!(!bytecode.contains("CallMethodValue"), "{name}: {bytecode}");
    let code_map = artifact_text(bundle, JitArtifactFileName::CodeMap);
    assert_eq!(
        code_map.matches("machineMethodIntrinsic").count(),
        sites,
        "{name}: {code_map}"
    );
    assert_eq!(
        code_map.matches("machineNativeLeafIdentity").count(),
        sites,
        "{name}: {code_map}"
    );
}

fn warmed(selection: JitSelection) -> Runtime {
    let mut runtime = runtime(selection);
    let result = run(&mut runtime, SETUP);
    if selection == JitSelection::ProductionTiered {
        for name in [
            "explicitAbs",
            "explicitMin",
            "explicitMax",
            "explicitEffect",
        ] {
            assert_explicit_native_artifact(&result, name, 1);
        }
    }
    runtime
}

fn assert_native_entry(before: RuntimeExecutionStats, after: RuntimeExecutionStats) {
    let entries = after.jit_optimized_entries - before.jit_optimized_entries
        + after.jit_generated_optimizing_entries
        - before.jit_generated_optimizing_entries;
    assert!(entries > 0, "probe must execute an optimizing body");
}

fn assert_hot(before: RuntimeExecutionStats, after: RuntimeExecutionStats) {
    assert_native_entry(before, after);
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(
        after.jit_generated_call_deopts,
        before.jit_generated_call_deopts
    );
    assert_eq!(after.jit_code_generations, before.jit_code_generations);
    assert_eq!(after.jit_feedback_refreshes, before.jit_feedback_refreshes);
    assert_eq!(
        (
            after.jit_to_rust_call_transitions - before.jit_to_rust_call_transitions,
            after.jit_reentrant_stub_transitions - before.jit_reentrant_stub_transitions,
            after.jit_runtime_property_stubs - before.jit_runtime_property_stubs,
        ),
        (0, 0, 0),
        "prepared Int32 calls must stay in generated code"
    );
}

#[test]
fn explicit_int32_math_hits_and_overflow_reuse_generated_code() {
    let mut runtime = warmed(JitSelection::ProductionTiered);
    let before = runtime.execution_stats();
    assert_eq!(
        completion(
            &mut runtime,
            "JSON.stringify([explicitAbs(explicitMath, -7), explicitMin(explicitMath, -7, 2), explicitMax(explicitMath, -7, 2)]);",
        ),
        "[7,-7,2]"
    );
    assert_hot(before, runtime.execution_stats());

    let before = runtime.execution_stats();
    assert_eq!(
        completion(&mut runtime, "explicitAbs(explicitMath, -2147483648);"),
        "2147483648"
    );
    let after = runtime.execution_stats();
    assert_native_entry(before, after);
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(
        after.jit_generated_call_deopts,
        before.jit_generated_call_deopts
    );
    assert!(after.jit_reentrant_stub_transitions > before.jit_reentrant_stub_transitions);
    let before = runtime.execution_stats();
    assert_eq!(
        completion(&mut runtime, "explicitAbs(explicitMath, -7);"),
        "7"
    );
    assert_hot(before, runtime.execution_stats());
}

#[test]
fn explicit_lookup_precedes_arguments_and_preserves_callee_and_receiver() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = warmed(selection);
        assert_eq!(
            completion(
                &mut runtime,
                r#"
const originalAbs = explicitMath.abs;
let order = "";
let replacements = 0;
function replacement(value) { replacements++; return this.marker + value; }
Object.defineProperty(explicitMath, "abs", { configurable: true, get() {
    order += "g";
    return originalAbs;
} });
const result = [explicitEffect(explicitMath, function() { order += "a"; return -7; }), order];
Object.defineProperty(explicitMath, "abs", { configurable: true, writable: true, value: originalAbs });
result.push(explicitEffect(explicitMath, function() {
    explicitMath.abs = replacement;
    return -7;
}));
result.push(explicitAbs(explicitMath, -7), replacements);
order = "";
try { explicitEffect(null, function() { order += "a"; return 1; }); }
catch (error) { result.push(error instanceof TypeError, order); }
const sentinel = {};
Object.defineProperty(explicitMath, "abs", { configurable: true, get() { order += "g"; throw sentinel; } });
try { explicitEffect(explicitMath, function() { order += "a"; return 1; }); }
catch (error) { result.push(error === sentinel, order); }
JSON.stringify(result);
"#
            ),
            r#"[7,"ga",7,34,1,true,"",true,"g"]"#,
            "{selection:?}"
        );
    }
}

const GC_SETUP: &str = r#"
let allocateArgument = false;
let argumentReceiver;
function argumentWithGc() {
    if (allocateArgument) {
        argumentReceiver.abs = Math.min;
        const retained = [];
        for (let index = 0; index < 128; index++) retained.push({ value: index });
        if (retained[127].value !== 127) throw new Error("argument roots lost");
    }
    return -7;
}
function explicitAllocatingArgument(receiver) { return receiver.abs(argumentWithGc() | 0); }
const allocationWarmReceiver = { marker: 50, abs: Math.abs };
for (let warm = 0; warm < 4010; warm++) explicitAllocatingArgument(allocationWarmReceiver);
function movingReceiver(offset) { return { marker: 50, abs(value) { return this.marker + value + offset; } }; }
allocateArgument = true;
"#;

fn assert_stress_relocation(before: RuntimeExecutionStats, after: RuntimeExecutionStats) {
    let stride = std::env::var("OTTER_GC_STRESS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(stride @ (1 | 4 | 16)) = stride {
        assert!(after.gc_minor_cycles - before.gc_minor_cycles >= 128 / stride);
        assert!(after.gc_minor_slot_updates > before.gc_minor_slot_updates);
    }
}

#[test]
fn explicit_loaded_callable_and_receiver_survive_argument_collections() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = runtime(selection);
        let setup = run(&mut runtime, GC_SETUP);
        if selection == JitSelection::ProductionTiered {
            assert_explicit_native_artifact(&setup, "explicitAllocatingArgument", 1);
        }
        let before = runtime.execution_stats();
        assert_eq!(
            completion(
                &mut runtime,
                // Create the captured callable after source compilation so it
                // is still young when argument evaluation drops its slot edge.
                "argumentReceiver = movingReceiver(5); explicitAllocatingArgument(argumentReceiver);"
            ),
            "48",
            "{selection:?}"
        );
        let after = runtime.execution_stats();
        assert_stress_relocation(before, after);
        if selection == JitSelection::ProductionTiered {
            assert_native_entry(before, after);
        }
    }
}

#[test]
fn explicit_number_and_coercion_misses_preserve_canonical_results() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = runtime(selection);
        assert_eq!(
            completion(
                &mut runtime,
                r#"
function identity(value) { return value; }
function rawAbs(receiver, value) { return receiver.abs(identity(value)); }
function rawMin(receiver, left, right) { return receiver.min(identity(left), identity(right)); }
function rawMax(receiver, left, right) { return receiver.max(identity(left), identity(right)); }
function allocateDuringCoercion() {
    const retained = [];
    for (let index = 0; index < 128; index++) retained.push({ value: index });
    if (retained[127].value !== 127) throw new Error("coercion roots lost");
}
let order = "";
const first = { valueOf() { order += "a"; allocateDuringCoercion(); return 7; } };
const later = { valueOf() { order += "b"; return 2; } };
const result = [rawMax(Math, first, later), order];
order = "";
const stringFirst = { valueOf() { order += "a"; return String(12345); } };
const allocatingLater = { valueOf() { order += "b"; allocateDuringCoercion(); return 7; } };
result.push(rawMax(Math, stringFirst, allocatingLater), order);
result.push(Object.is(rawAbs(Math, -0), 0), Object.is(rawMin(Math, 0, -0), -0));
result.push(Object.is(rawMax(Math, -0, 0), 0), Number.isNaN(rawMax(Math, 1, NaN)));
JSON.stringify(result);
"#
            ),
            r#"[7,"ab",12345,"ab",true,true,true,true]"#,
            "{selection:?}"
        );
    }
}

#[test]
fn namespace_property_proofs_are_prepared_before_first_optimizing_loop() {
    let mut runtime = runtime(JitSelection::ProductionTiered);
    let setup = run(
        &mut runtime,
        r#"
function explicitMathNamespaceLoop(limit) {
    let checksum = 0;
    for (let index = 0; index < limit; index++) {
        checksum += Math.abs((index & 15) - 8);
        checksum += Math.min((index & 15) - 8, 2);
        checksum += Math.max((index & 15) - 8, 2);
    }
    return checksum;
}
for (let warm = 0; warm < 4010; warm++) explicitMathNamespaceLoop(16);
"#,
    );
    assert_explicit_native_artifact(&setup, "explicitMathNamespaceLoop", 3);
    let bundle = setup
        .jit_artifacts()
        .expect("namespace artifacts")
        .bundles()
        .iter()
        .find(|bundle| {
            bundle.manifest().function_name() == "explicitMathNamespaceLoop"
                && bundle.manifest().tier() == JitDebugTier::Optimizing
        })
        .expect("namespace optimizing body");
    let code_map = artifact_text(bundle, JitArtifactFileName::CodeMap);
    assert_eq!(
        code_map.matches("machineCacheIrLoadField").count(),
        3,
        "{code_map}"
    );
    let before = runtime.execution_stats();
    assert_eq!(
        completion(&mut runtime, "explicitMathNamespaceLoop(128);"),
        "704"
    );
    assert_hot(before, runtime.execution_stats());
}
