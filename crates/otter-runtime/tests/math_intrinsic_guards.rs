//! Regression coverage for guarded `Math.<method>(...)` intrinsic calls.
//!
//! # Contents
//! - Bootstrap calls and observable global, lexical, and method replacement.
//! - Object-to-primitive coercion on the generic boundary.
//! - Optimizing Int32 `abs` / `max` / `min`, including `INT32_MIN` and a
//!   post-tier-up method replacement.
//! - Shared Machine method hits, cold coercion/Number cases, and receiver guards.
//! - Mapped argument methods read the current parameter cell on the cold path.
//! - Reentrant coercion roots pending objects and already-coerced strings.
//! - Numeric conversion stops on the first error and respects builtin arity.
//!
//! # Invariants
//! - Fast paths require the exact bootstrap method identity.
//! - User-visible coercion and replacement remain observable.
//! - Optimizing results match the interpreter oracle exactly.
//! - Warm method hits use generated code without per-call runtime transitions.
//!
//! # See also
//! - `otter_jit::machine` for shared guarded Math method lowering.

use otter_runtime::{
    JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Otter, Runtime,
    RuntimeExecutionStats, SourceInput,
};

fn run(source: &str) {
    Otter::new()
        .blocking_run_script(source)
        .expect("script should run");
}

#[test]
fn math_call_uses_original_method_fast_path() {
    run(r#"
            if (Math.sqrt(9) !== 3) {
                throw new Error("bad sqrt");
            }
        "#);
}

#[test]
fn math_call_observes_method_overwrite() {
    run(r#"
            Math.sqrt = () => 7;
            if (Math.sqrt(9) !== 7) {
                throw new Error("overwritten Math.sqrt ignored");
            }
        "#);
}

#[test]
fn math_call_observes_global_replacement() {
    run(r#"
            globalThis.Math = { sqrt() { return 13; } };
            if (Math.sqrt(9) !== 13) {
                throw new Error("global Math replacement ignored");
            }
        "#);
}

#[test]
fn math_call_observes_lexical_shadow() {
    run(r#"
            let Math = { sqrt() { return 11; } };
            if (Math.sqrt(9) !== 11) {
                throw new Error("lexical Math shadow ignored");
            }
        "#);
}

#[test]
fn math_call_preserves_object_to_primitive() {
    run(r#"
            let hits = 0;
            const value = {
                valueOf() {
                    hits += 1;
                    return 9;
                }
            };
            if (Math.sqrt(value) !== 3 || hits !== 1) {
                throw new Error("object coercion skipped");
            }
        "#);
}

const OPTIMIZING_INT32_MATH: &str = r#"
function int32Math(limit) {
    let checksum = 0;
    for (let index = 0; index < limit; index++) {
        const value = (index & 15) - 8;
        checksum += Math.abs(value);
        checksum += Math.max(value, 2);
        checksum += Math.min(value, 2);
    }
    return checksum;
}

for (let warm = 0; warm < 4010; warm++) int32Math(16);
const beforeReplacement = [
    int32Math(128),
    Math.abs((-2147483647 - 1) | 0),
    Math.max(-7, 2),
    Math.min(-7, 2)
];
Math.abs = value => value + 1000;
const afterReplacement = [
    int32Math(4),
    Math.abs((-2147483647 - 1) | 0),
    Math.max(-7, 2),
    Math.min(-7, 2)
];
JSON.stringify([beforeReplacement, afterReplacement]);
"#;

fn run_int32_math(selection: JitSelection) -> (String, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(OPTIMIZING_INT32_MATH),
            "optimizing-int32-math.js",
        )
        .expect("Int32 Math matrix")
        .completion_string()
        .to_owned();
    let optimized_entries = runtime.execution_stats().jit_optimized_entries;
    (completion, optimized_entries)
}

#[test]
fn optimizing_int32_math_matches_interpreter_and_observes_replacement() {
    let (oracle, _) = run_int32_math(JitSelection::InterpreterOnly);
    let (compiled, optimized_entries) = run_int32_math(JitSelection::ProductionTiered);

    assert_eq!(compiled, oracle);
    assert_eq!(oracle, r#"[[704,2147483648,2,-7],[3956,-2147482648,2,-7]]"#);
    assert!(
        optimized_entries > 0,
        "fixture must enter optimizing code before replacing Math.abs"
    );
}

const GUARDED_MATH_SETUP: &str = r#"
function guardedMathAbs(receiver, value) { return receiver.abs(value); }
function guardedMathMin(receiver, left, right) { return receiver.min(left, right); }
function guardedMathMax(receiver, left, right) { return receiver.max(left, right); }
function guardedInt32MathAbs(receiver, value) {
    const integer = value | 0;
    return receiver.abs(integer);
}
function guardedInt32MathMin(receiver, left, right) {
    const a = left | 0;
    const b = right | 0;
    return receiver.min(a, b);
}
function guardedInt32MathMax(receiver, left, right) {
    const a = left | 0;
    const b = right | 0;
    return receiver.max(a, b);
}
const guardedInheritedMath = Object.create(Math);
function guardedInt32InheritedAbs(receiver, value) {
    const integer = value | 0;
    return receiver.abs(integer);
}
function guardedMathLoop(receiver, limit) {
    let checksum = 0;
    for (let index = 0; index < limit; index++) {
        const value = (index & 15) - 8;
        checksum += receiver.abs(value);
        checksum += receiver.min(value, 2);
        checksum += receiver.max(value, 2);
    }
    return checksum;
}
for (let warm = 0; warm < 4010; warm++) {
    guardedMathAbs(Math, -7);
    guardedMathMin(Math, -7, 2);
    guardedMathMax(Math, -7, 2);
    guardedInt32MathAbs(Math, -7);
    guardedInt32MathMin(Math, -7, 2);
    guardedInt32MathMax(Math, -7, 2);
    guardedInt32InheritedAbs(guardedInheritedMath, -7);
    guardedMathLoop(Math, 16);
}
"#;

fn math_completion(runtime: &mut Runtime, source: &str, module: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), module)
        .unwrap_or_else(|error| panic!("Math fixture {module}: {error:?}"))
        .completion_string()
        .to_owned()
}

#[test]
fn guarded_math_method_hits_stay_in_machine_code() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .expect("Math method artifact runtime");
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(GUARDED_MATH_SETUP),
            "guarded-math-method-setup.js",
        )
        .expect("warm Math method functions");
    let bundle = setup
        .jit_artifacts()
        .expect("enabled artifact batch")
        .bundles()
        .iter()
        .rev()
        .find(|bundle| {
            bundle.manifest().function_name() == "guardedMathLoop"
                && bundle.manifest().tier() == JitDebugTier::Optimizing
        })
        .unwrap_or_else(|| panic!("Math loop must optimize: {:?}", setup.jit_debug_report()));
    let code_map: serde_json::Value = serde_json::from_slice(
        bundle
            .file(JitArtifactFileName::CodeMap)
            .expect("Math code map")
            .contents(),
    )
    .expect("valid Math code map");
    let method_hits = code_map["regions"]
        .as_array()
        .expect("code-map regions")
        .iter()
        .filter(|region| region["kind"] == "machineMethodIntrinsic")
        .count();
    assert_eq!(
        method_hits, 3,
        "abs/min/max must each have a generated Machine method hit: {code_map}"
    );
    let relocations = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::Relocations)
            .expect("Math relocations")
            .contents(),
    )
    .expect("relocations are UTF-8");
    for leaf in ["math_abs_leaf", "math_min_leaf", "math_max_leaf"] {
        assert!(
            !relocations.contains(leaf),
            "the generated Math hit must replace {leaf}: {relocations}"
        );
    }
    drop(setup);

    let before = runtime.execution_stats();
    assert_eq!(
        math_completion(
            &mut runtime,
            "guardedMathLoop(Math, 128);",
            "guarded-math-method-probe.js",
        ),
        "704"
    );
    let after = runtime.execution_stats();
    assert!(after.jit_optimized_entries > before.jit_optimized_entries);
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert_eq!(after.jit_code_generations, before.jit_code_generations);
    assert_eq!(
        (
            after.jit_runtime_calls - before.jit_runtime_calls,
            after.jit_to_rust_call_transitions - before.jit_to_rust_call_transitions,
            after.jit_reentrant_stub_transitions - before.jit_reentrant_stub_transitions,
            after.jit_runtime_property_stubs - before.jit_runtime_property_stubs,
        ),
        (0, 0, 0, 0),
        "384 Int32 Math calls must not enter a canonical runtime boundary"
    );
}

fn warmed_math_runtime(selection: JitSelection) -> Runtime {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .expect("Math method runtime");
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(GUARDED_MATH_SETUP),
            "guarded-math-matrix-setup.js",
        )
        .expect("warm Math matrix");
    if selection == JitSelection::ProductionTiered {
        let artifacts = setup.jit_artifacts().expect("Math matrix artifacts");
        for name in [
            "guardedInt32MathAbs",
            "guardedInt32MathMin",
            "guardedInt32MathMax",
            "guardedInt32InheritedAbs",
        ] {
            let bundle = artifacts
                .bundles()
                .iter()
                .rev()
                .find(|bundle| {
                    bundle.manifest().function_name() == name
                        && bundle.manifest().tier() == JitDebugTier::Optimizing
                })
                .unwrap_or_else(|| {
                    panic!(
                        "{name} must optimize before probes: {:?}",
                        setup.jit_debug_report()
                    )
                });
            let code_map = std::str::from_utf8(
                bundle
                    .file(JitArtifactFileName::CodeMap)
                    .expect("Math code map")
                    .contents(),
            )
            .expect("code map is UTF-8");
            assert!(
                code_map.contains("machineMethodIntrinsic"),
                "{name}: {code_map}"
            );
            assert_eq!(
                code_map.matches("machineCacheIrGuardOrdinaryState").count(),
                if name == "guardedInt32InheritedAbs" {
                    2
                } else {
                    1
                },
                "every receiver and holder must prove ordinary descriptor state: {name}: {code_map}"
            );
            if name == "guardedInt32InheritedAbs" {
                assert!(
                    code_map.contains("machineCacheIrLoadPrototype"),
                    "the inherited site must guard its holder before replacement: {code_map}"
                );
            }
        }
    }
    drop(setup);
    runtime
}

fn guarded_math_matrix(source: &str) -> String {
    let mut results = Vec::new();
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = warmed_math_runtime(selection);
        results.push(math_completion(
            &mut runtime,
            source,
            "guarded-math-matrix-probe.js",
        ));
    }
    assert_eq!(results[0], results[1]);
    results.remove(0)
}

fn assert_math_method_probe(
    runtime: &mut Runtime,
    source: &str,
    expected: &str,
    expect_cold: bool,
) {
    let before = runtime.execution_stats();
    assert_eq!(
        math_completion(runtime, source, "guarded-math-reuse-probe.js"),
        expected
    );
    let after = runtime.execution_stats();
    assert_math_method_delta(before, after, source, expect_cold);
}

fn assert_math_method_delta(
    before: RuntimeExecutionStats,
    after: RuntimeExecutionStats,
    source: &str,
    expect_cold: bool,
) {
    let native_entries = after.jit_optimized_entries - before.jit_optimized_entries
        + after.jit_generated_optimizing_entries
        - before.jit_generated_optimizing_entries;
    assert!(
        native_entries > 0,
        "the probe must enter Machine code: {source}"
    );
    assert_eq!(
        after.jit_optimized_deopts, before.jit_optimized_deopts,
        "{source}"
    );
    assert_eq!(
        after.jit_generated_call_deopts, before.jit_generated_call_deopts,
        "{source}"
    );
    if expect_cold {
        assert!(
            after.jit_reentrant_stub_transitions > before.jit_reentrant_stub_transitions,
            "the miss must use the committed method boundary: {source}"
        );
    } else {
        assert_eq!(
            after.jit_code_generations, before.jit_code_generations,
            "{source}"
        );
        assert_eq!(
            (
                after.jit_runtime_calls - before.jit_runtime_calls,
                after.jit_to_rust_call_transitions - before.jit_to_rust_call_transitions,
                after.jit_reentrant_stub_transitions - before.jit_reentrant_stub_transitions,
            ),
            (0, 0, 0),
            "the same Machine generation must regain its hot hit: {source}"
        );
    }
}

#[test]
fn guarded_math_method_cold_misses_rejoin_without_deopt() {
    let mut runtime = warmed_math_runtime(JitSelection::ProductionTiered);
    assert_math_method_probe(
        &mut runtime,
        "guardedInt32MathAbs(Math, -2147483648);",
        "2147483648",
        true,
    );
    assert_math_method_probe(&mut runtime, "guardedInt32MathAbs(Math, -7);", "7", false);
    math_completion(
        &mut runtime,
        "var guardedOriginalAbs = Math.abs; Math.abs = Math.max;",
        "guarded-math-replace.js",
    );
    assert_math_method_probe(&mut runtime, "guardedInt32MathAbs(Math, -7);", "-7", true);
    assert_math_method_probe(
        &mut runtime,
        "guardedInt32InheritedAbs(guardedInheritedMath, -7);",
        "-7",
        true,
    );
    math_completion(
        &mut runtime,
        "Math.abs = guardedOriginalAbs;",
        "guarded-math-restore.js",
    );
    assert_math_method_probe(&mut runtime, "guardedInt32MathAbs(Math, -7);", "7", false);
    assert_math_method_probe(
        &mut runtime,
        "guardedInt32InheritedAbs(guardedInheritedMath, -7);",
        "7",
        false,
    );
}

#[test]
fn guarded_math_method_accessor_overrides_reenter_once_without_deopt() {
    let mut runtime = warmed_math_runtime(JitSelection::ProductionTiered);
    math_completion(
        &mut runtime,
        r#"
var guardedAbsDescriptor = Object.getOwnPropertyDescriptor(Math, "abs");
var guardedAccessorBuiltin = Math.abs;
var guardedGetterCalls = 0;
Object.defineProperty(Math, "abs", {
    configurable: true,
    get() { guardedGetterCalls++; return guardedAccessorBuiltin; }
});
"#,
        "guarded-math-accessor-install.js",
    );
    for (index, source) in [
        "guardedInt32MathAbs(Math, -7);",
        "guardedInt32InheritedAbs(guardedInheritedMath, -7);",
        "guardedInt32MathAbs(Math, -7);",
        "guardedInt32InheritedAbs(guardedInheritedMath, -7);",
    ]
    .into_iter()
    .enumerate()
    {
        let before = runtime.execution_stats();
        let result = math_completion(&mut runtime, source, "guarded-math-accessor-probe.js");
        let after = runtime.execution_stats();
        assert_eq!(result, "7");
        assert_eq!(
            math_completion(
                &mut runtime,
                "guardedGetterCalls;",
                "guarded-math-accessor-current-count.js",
            ),
            (index + 1).to_string(),
            "the getter must execute once before considering boundary telemetry: {source}"
        );
        if index < 2 {
            assert_math_method_delta(before, after, source, true);
        } else {
            // The first cold lookup records changed holder shapes and may
            // invalidate that caller through the ordinary feedback policy.
            assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
            assert_eq!(
                after.jit_generated_call_deopts,
                before.jit_generated_call_deopts
            );
        }
    }
    assert_eq!(
        math_completion(
            &mut runtime,
            "guardedGetterCalls;",
            "guarded-math-accessor-count.js"
        ),
        "4",
        "both warmed receivers must perform the observable getter once per call"
    );
    math_completion(
        &mut runtime,
        "Object.defineProperty(Math, \"abs\", guardedAbsDescriptor);",
        "guarded-math-accessor-restore.js",
    );
    // Descriptor override history may conservatively keep the old shape cold.
    assert_eq!(
        math_completion(
            &mut runtime,
            "JSON.stringify([guardedInt32MathAbs(Math, -7), guardedInt32InheritedAbs(guardedInheritedMath, -7), guardedGetterCalls]);",
            "guarded-math-accessor-restored.js",
        ),
        "[7,7,4]"
    );
}

#[test]
fn guarded_math_method_mapped_arguments_observe_current_parameter() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .build()
            .expect("mapped Math method runtime");
        let setup = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
function makeMappedMath(value) {
    return { receiver: arguments, replace(next) { value = next; } };
}
function invokeMappedMath(receiver) {
    const n = -7;
    return receiver["0"](n);
}
const mappedMath = makeMappedMath(Math.abs);
for (let warm = 0; warm < 4010; warm++) invokeMappedMath(mappedMath.receiver);
"#,
                ),
                "guarded-math-mapped-setup.js",
            )
            .expect("warm mapped Math method");
        if selection == JitSelection::ProductionTiered {
            let bundle = setup
                .jit_artifacts()
                .expect("mapped Math artifacts")
                .bundles()
                .iter()
                .rev()
                .find(|bundle| {
                    bundle.manifest().function_name() == "invokeMappedMath"
                        && bundle.manifest().tier() == JitDebugTier::Optimizing
                })
                .unwrap_or_else(|| {
                    panic!(
                        "mapped Math caller must optimize: {:?}",
                        setup.jit_debug_report()
                    )
                });
            let code_map = std::str::from_utf8(
                bundle
                    .file(JitArtifactFileName::CodeMap)
                    .expect("mapped Math code map")
                    .contents(),
            )
            .expect("code map is UTF-8");
            assert!(
                code_map.contains("machineMethodIntrinsic"),
                "the mapped receiver must exercise the guarded Math probe: {code_map}"
            );
        }
        drop(setup);
        assert_eq!(
            math_completion(
                &mut runtime,
                "invokeMappedMath(mappedMath.receiver);",
                "guarded-math-mapped-original.js",
            ),
            "7"
        );
        // Updating the captured parameter changes its UpvalueCell without
        // changing the arguments object's shape or its original slot value.
        math_completion(
            &mut runtime,
            "mappedMath.replace(Math.max);",
            "guarded-math-mapped-replace.js",
        );
        let before = runtime.execution_stats();
        assert_eq!(
            math_completion(
                &mut runtime,
                "invokeMappedMath(mappedMath.receiver);",
                "guarded-math-mapped-current.js",
            ),
            "-7",
            "{selection:?}: the mapped parameter value must override the stale abs slot"
        );
        let after = runtime.execution_stats();
        if selection == JitSelection::ProductionTiered {
            assert_math_method_delta(before, after, "mapped Math.max replacement", true);
        }
    }
}

#[test]
fn math_argument_coercion_roots_pending_objects_and_returned_strings() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("Math coercion rooting runtime");
        assert_eq!(
            math_completion(
                &mut runtime,
                r#"
function allocateDuringCoercion() {
    const retained = [];
    for (let index = 0; index < 128; index++) retained.push({ value: index });
    if (retained[127].value !== 127) throw new Error("coercion roots lost");
}
function pendingObjectArgument() {
    let order = "";
    const left = { valueOf() { order += "a"; allocateDuringCoercion(); return 7; } };
    const right = { valueOf() { order += "b"; return 2; } };
    return [Math.max(left, right), order];
}
function alreadyCoercedString() {
    let order = "";
    const left = { valueOf() { order += "a"; return String(12345); } };
    const right = { valueOf() { order += "b"; allocateDuringCoercion(); return 7; } };
    return [Math.max(left, right), order];
}
JSON.stringify([pendingObjectArgument(), alreadyCoercedString()]);
"#,
                "math-coercion-roots.js",
            ),
            r#"[[7,"ab"],[12345,"ab"]]"#,
            "{selection:?}: every coercion must execute once with live argument roots"
        );
    }
}

#[test]
fn math_argument_coercion_preserves_error_order_and_ignores_extra_operands() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("Math coercion ordering runtime");
        assert_eq!(
            math_completion(
                &mut runtime,
                r#"
let order = "";
const sentinel = {};
function failingFirst(method, first, throwsSentinel) {
    order = "";
    let caught = false;
    const later = { valueOf() { order += "b"; return 1; } };
    try { method(first, later); }
    catch (error) {
        caught = throwsSentinel ? error === sentinel : error instanceof TypeError;
    }
    return [caught, order];
}
const result = [
    failingFirst(Math.max, Symbol("first"), false),
    failingFirst(Math.min, 1n, false),
    failingFirst(Math.max, { valueOf() { order += "a"; return Symbol("first"); } }, false),
    failingFirst(Math.min, { valueOf() { order += "a"; return 1n; } }, false),
    failingFirst(Math.max, { valueOf() { order += "a"; throw sentinel; } }, true)
];
function extraArgument() {
    order += "e";
    return { valueOf() { order += "x"; throw new Error("ignored operand converted"); } };
}
order = "";
result.push([Math.abs(-3, extraArgument()), order]);
order = "";
result.push([Math.f16round(1.5, extraArgument()), order]);
order = "";
result.push([Math.pow(2, 3, extraArgument()), order]);
order = "";
result.push([Math.imul(3, 4, extraArgument()), order]);
JSON.stringify(result);
"#,
                "math-coercion-order.js",
            ),
            r#"[[true,""],[true,""],[true,"a"],[true,"a"],[true,"a"],[3,"e"],[1.5,"e"],[8,"e"],[12,"e"]]"#,
            "{selection:?}: ToNumber stops before later conversions; ignored operands are evaluated only"
        );
    }
}

#[test]
fn guarded_math_method_number_misses_coerce_once_and_preserve_number_semantics() {
    let result = guarded_math_matrix(
        r#"
const result = [
    guardedMathAbs(Math, (-2147483647 - 1) | 0),
    Object.is(guardedMathAbs(Math, -0), 0),
    Object.is(guardedMathMin(Math, -0, 0), -0),
    Object.is(guardedMathMin(Math, 0, -0), -0),
    Object.is(guardedMathMax(Math, -0, 0), 0),
    Object.is(guardedMathMax(Math, 0, -0), 0),
    Number.isNaN(guardedMathAbs(Math, NaN)),
    Number.isNaN(guardedMathMin(Math, NaN, 1)),
    Number.isNaN(guardedMathMax(Math, 1, NaN))
];
let absCoercions = 0;
result.push(guardedMathAbs(Math, { valueOf() { absCoercions++; return -9; } }));
result.push(absCoercions);
let order = "";
const left = { valueOf() { order += "a"; return 7; } };
const right = { valueOf() { order += "b"; return 2; } };
result.push(guardedMathMin(Math, left, right));
result.push(guardedMathMax(Math, left, right));
result.push(order);
let throwCoercions = 0;
let caught = false;
try {
    guardedMathAbs(Math, { valueOf() { throwCoercions++; throw "coercion"; } });
} catch (error) {
    caught = error === "coercion";
}
result.push(caught, throwCoercions);
JSON.stringify(result);
"#,
    );
    assert_eq!(
        result,
        r#"[2147483648,true,true,true,true,true,true,true,true,9,1,2,7,"abab",true,1]"#
    );
}

#[test]
fn guarded_math_method_identity_misses_observe_own_prototype_and_global_replacement() {
    let result = guarded_math_matrix(
        r#"
const originalMath = Math;
const originalAbs = Math.abs;
let calls = 0;
const result = [guardedInt32InheritedAbs(guardedInheritedMath, -7)];
Math.abs = function(value) { calls++; return value + 100; };
result.push(guardedInt32MathAbs(Math, -7));
result.push(guardedInt32InheritedAbs(guardedInheritedMath, -7), calls);
Math.abs = originalAbs;
result.push(guardedInt32InheritedAbs(guardedInheritedMath, -7));
guardedInheritedMath.abs = function(value) { calls++; return value + 200; };
result.push(guardedInt32InheritedAbs(guardedInheritedMath, -7));
delete guardedInheritedMath.abs;
result.push(guardedInt32InheritedAbs(guardedInheritedMath, -7));
Math.abs = function(value) { calls++; return value + 300; };
result.push(guardedInt32InheritedAbs(guardedInheritedMath, -7));
Math.abs = originalAbs;
globalThis.Math = {
    abs(value) { calls++; return value + 400; },
    min(left, right) { calls++; return 701; },
    max(left, right) { calls++; return 702; }
};
result.push(guardedInt32MathAbs(Math, -7));
result.push(guardedInt32MathMin(Math, -7, 2));
result.push(guardedInt32MathMax(Math, -7, 2));
result.push(calls);
globalThis.Math = originalMath;
JSON.stringify(result);
"#,
    );
    assert_eq!(result, "[7,93,93,2,7,193,7,293,393,701,702,7]");
}
